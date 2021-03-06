pub mod queues;
mod uniform_manager;
pub mod shader;
pub mod renderer_error;

use winit::dpi::PhysicalSize;
use vulkano::swapchain::SwapchainAcquireFuture;
use crate::renderer::shader::fragment_shader::ty::PushConstants;
use vulkano::command_buffer::AutoCommandBuffer;
use crate::renderer::renderer_error::RenderError;
use vulkano::framebuffer::FramebufferCreationError;
use vulkano::pipeline::GraphicsPipelineCreationError;
use crate::renderer::renderer_error::RendererCreationError;
use vulkano::format::Format;
use vulkano::framebuffer::RenderPassCreationError;
use vulkano::device::DeviceCreationError;
use vulkano::device::QueuesIter;
use vulkano::instance::QueueFamily;
use vulkano::image::attachment::AttachmentImage;
use crate::resource::AssetManager;
use std::cell::RefCell;
use std::rc::Rc;
use log::*;

use crate::settings::Settings;

use vulkano::instance::{Instance, InstanceCreationError, PhysicalDevice, PhysicalDeviceType, PhysicalDevicesIter};
use vulkano::descriptor::descriptor_set::PersistentDescriptorSet;
use vulkano::command_buffer::{AutoCommandBufferBuilder, DynamicState};
use vulkano::device::{Device};
use vulkano::pipeline::{GraphicsPipeline, GraphicsPipelineAbstract};
use vulkano::pipeline::viewport::Viewport;
use vulkano::image::SwapchainImage;
use vulkano::swapchain::{Surface, PresentMode, Swapchain, SurfaceTransform, CompositeAlpha};
use vulkano::single_pass_renderpass;
use vulkano::framebuffer::{RenderPassAbstract, Framebuffer, FramebufferAbstract, Subpass};
use winit::{EventsLoop, WindowBuilder, Window};
use vulkano::sync::GpuFuture;
use vulkano::sync;
use vulkano::swapchain::{AcquireError};
use vulkano::swapchain;

use vulkano_win::VkSurfaceBuild;

use std::sync::Arc;

use crate::renderer::queues::Queues;
use crate::renderer::uniform_manager::UniformManager;
use crate::renderer::shader::ShaderSet;

/// Top level struct of vulkan renderer.
pub struct Renderer {
    instance: Arc<Instance>,
    surface: Arc<Surface<Window>>,
    device: Arc<Device>,
    queues: Queues,
    swapchain: Arc<Swapchain<Window>>,
    images: Vec<Arc<SwapchainImage<Window>>>,
    uniform_manager: UniformManager,
    shader_set: Rc<ShaderSet>,
    render_pass: Arc<RenderPassAbstract + Send + Sync>,
    pipeline: Arc<GraphicsPipelineAbstract + Send + Sync>,
    framebuffers: Vec<Arc<FramebufferAbstract + Send + Sync>>,

    recreate_swapchain: bool,
    previous_frame: Option<Box<GpuFuture>>,
}

impl Renderer {
    /// Creates new renderer.
    pub fn new(settings: &Settings, events_loop: &EventsLoop) -> Result<Self, RendererCreationError> {
        let instance = create_new_instance()?;

        let physical_device = rank_devices(PhysicalDevice::enumerate(&instance))?;
        info!("Using device: {} (type: {:?})", physical_device.name(), physical_device.ty());

        let surface = WindowBuilder::new().with_title(settings.window_title())
                                          .with_dimensions(settings.initial_window_size().to_logical(1.0))
                                          .build_vk_surface(events_loop, instance.clone())?;
        let window = surface.window();

        let physical_queues = queues::find_queues(physical_device, &surface);

        let (device, queues) = create_logical_device(physical_device, &physical_queues)?;

        let queues = Queues::new(queues);

        let (swapchain, images) = create_swapchain(surface.clone(), physical_device, device.clone(), &queues)?;

        let uniform_manager = UniformManager::new(device.clone());
        let shader_set = Rc::new(ShaderSet::load(device.clone()));

        let render_pass = create_renderpass(device.clone(), swapchain.format())?;

        let pipeline = create_pipeline(device.clone(), shader_set.clone(), &images, render_pass.clone())?;
        let framebuffers = create_framebuffers(device.clone(), &images, render_pass.clone())?;

        Ok(Renderer {
            instance,
            surface,
            device: device.clone(),
            queues,
            swapchain,
            images,
            uniform_manager,
            shader_set,
            render_pass,
            pipeline,
            framebuffers,
            recreate_swapchain: false,
            previous_frame: None,
        })
    }

    /// Forces renderer to recreate swapchain.
    pub fn force_recreate_swapchain(&mut self) {
        self.recreate_swapchain = true;
    }

    /// Renders one frame using active scene from asset manager.
    pub fn render_scene(&mut self, command_buffer: AutoCommandBufferBuilder, asset_manager: &mut AssetManager) -> Result<(usize, SwapchainAcquireFuture<winit::Window>, AutoCommandBufferBuilder), RenderError> {
        if let Some(previous_frame) = &mut self.previous_frame {
            previous_frame.cleanup_finished();
        }

        if self.recreate_swapchain {
            self.recreate_swapchain()?;
        }

        let (image_num, acquire_future) = match swapchain::acquire_next_image(self.swapchain.clone(), None) {
            Ok(r) => r,
            Err(AcquireError::OutOfDate) => {
                self.recreate_swapchain = true;
                return Err(RenderError::AcquireError(AcquireError::OutOfDate))
            },
            Err(err) => return Err(RenderError::AcquireError(err)),
        };

        let command_buffer = self.add_scene_commands(command_buffer, image_num, asset_manager)?;

        Ok((image_num, acquire_future, command_buffer))
    }

    /// Executes commands stored in command buffer.
    pub fn execute_command_buffer(&mut self, image_num: usize, acquire_future: SwapchainAcquireFuture<winit::Window>, command_buffer: AutoCommandBufferBuilder) -> Result<(), RenderError> {
        let command_buffer = command_buffer.end_render_pass()?.build()?;
        
        let future = self.previous_frame.take()
                                .unwrap_or_else(|| Box::new(sync::now(self.device.clone())) as Box<_>)
                                .join(acquire_future)
                                .then_execute(self.queues.graphics_queue(), command_buffer)?
                                .then_swapchain_present(self.queues.graphics_queue(), self.swapchain.clone(), image_num)
                                .then_signal_fence_and_flush();

        match future {
            Ok(future) => {
                self.previous_frame = Some(Box::new(future) as Box<_>);
                Ok(())
            }
            Err(sync::FlushError::OutOfDate) => {
                self.recreate_swapchain = true;
                return Err(RenderError::FlushError(sync::FlushError::OutOfDate))
            }
            Err(e) => {
                return Err(RenderError::FlushError(e))
            }
        }   
    }

    /// Creates vulkan command buffer.
    pub fn create_command_buffer(&mut self) -> Result<AutoCommandBufferBuilder, RenderError> {
        Ok(AutoCommandBufferBuilder::primary_one_time_submit(self.device.clone(), self.queues.graphics_queue().family())?)
    }

    /// Adds commands used to draw current scene to command buffer.
    fn add_scene_commands(&mut self, mut command_buffer: AutoCommandBufferBuilder, image_num: usize, asset_manager: &mut AssetManager) -> Result<AutoCommandBufferBuilder, RenderError> {
        command_buffer = command_buffer.begin_render_pass(
            self.framebuffers[image_num].clone(), false,
            vec![
                [0.0, 0.0, 0.0, 1.0].into(),
                1f32.into(),
            ]
        )?;

        if let Some(scene) = asset_manager.active_scene() {
            let window_dimensions = get_window_dimensions(self.surface.window());
            let mut transformation_uniform_data = scene.camera().as_uniform_data(window_dimensions.width as f32, window_dimensions.height as f32);
            self.uniform_manager.update_light_data(scene.light_data());
            

            for object in scene.objects() {
                transformation_uniform_data.model = object.model_matrix().into();
                self.uniform_manager.update_transformation_data(transformation_uniform_data);
                let transformation_data_buffer_subbuffer = self.uniform_manager.get_transformation_subbuffer_data()?;
                let light_data_buffer_subbuffer = self.uniform_manager.get_light_subbuffer_data()?;

                let descriptor_set = PersistentDescriptorSet::start(self.pipeline.clone(), 0)
                                                             .add_buffer(transformation_data_buffer_subbuffer)?
                                                             .add_buffer(light_data_buffer_subbuffer)?;

                let push_constants = PushConstants {
                    light_source: object.light_source() as u32,
                    uniform_scale: object.uniform_scale() as u32,
                };

                if let Some(mesh) = object.mesh() {
                    let (mesh_texture, vertex_buffer, index_buffer) = {
                        let mesh = mesh.read().unwrap();
                        (mesh.texture(), mesh.vertex_buffer(), mesh.index_buffer())
                    };
                    let descriptor_set = descriptor_set.add_sampled_image(mesh_texture.image_buffer(), mesh_texture.sampler())?.build()?;
                    command_buffer = command_buffer.draw_indexed(
                        self.pipeline.clone(), 
                        &DynamicState::none(), 
                        vec!(vertex_buffer),
                        index_buffer, 
                        descriptor_set,
                        push_constants,
                    )?;
                }
            }
        }   

        Ok(command_buffer)
    }

    /// Recreates swapchain when surface changed.
    fn recreate_swapchain(&mut self) -> Result<(), RenderError>{
        let window_dimensions: (u32, u32) = get_window_dimensions(self.surface.window()).into();

        let (new_swapchain, new_images) = self.swapchain.recreate_with_dimension([window_dimensions.0, window_dimensions.1])?;

        self.swapchain = new_swapchain;
        self.images = new_images;

        self.pipeline = create_pipeline(self.device.clone(), self.shader_set.clone(), &self.images, self.render_pass.clone())?;
        self.framebuffers = create_framebuffers(self.device.clone(), &self.images, self.render_pass.clone())?;

        self.recreate_swapchain = false;
        Ok(())
    }

    /// Returns vulkan queues.
    pub fn queues(&self) -> Queues {
        self.queues.clone()
    }

    /// Returns vulkan device.
    pub fn device(&self) -> Arc<Device> {
        self.device.clone()
    }

    pub fn surface(&self) -> Arc<Surface<Window>> {
        self.surface.clone()
    }

    pub fn render_pass(&self) -> Arc<RenderPassAbstract + Send + Sync> {
        self.render_pass.clone()
    }

    pub fn framebuffer(&self, image_num: usize) -> Arc<FramebufferAbstract + Send + Sync> {
        self.framebuffers[image_num].clone()
    }

}

/// Creates framebuffers, which contain list of images that are attached.
fn create_framebuffers(
    device: Arc<Device>,
    images: &[Arc<SwapchainImage<Window>>], 
    render_pass: Arc<RenderPassAbstract + Send + Sync>
) -> Result<Vec<Arc<FramebufferAbstract + Send + Sync>>, FramebufferCreationError> {

    let dimensions = images[0].dimensions();
    let depth_buffer = AttachmentImage::transient(device, dimensions, Format::D16Unorm)
                                       .expect("Couldn't create depth buffer!");

    let mut framebuffers = Vec::with_capacity(images.len());

    for image in images {
        let framebuffer = Framebuffer::start(render_pass.clone())
                                                        .add(image.clone())?
                                                        .add(depth_buffer.clone())?
                                                        .build()?;
        framebuffers.push(Arc::new(framebuffer) as Arc<FramebufferAbstract + Send + Sync>);
    }

    Ok(framebuffers)
}

/// Creates a pipeline, which describe a graphical or computer operation.
fn create_pipeline(
    device: Arc<Device>, 
    shader_set: Rc<ShaderSet>, 
    images: &[Arc<SwapchainImage<Window>>], 
    render_pass: Arc<RenderPassAbstract + Send + Sync>
) -> Result<Arc<GraphicsPipelineAbstract + Send + Sync>, GraphicsPipelineCreationError> {
    
    let dimensions = images[0].dimensions();

    let pipeline = GraphicsPipeline::start()
        .vertex_input(ShaderSet::vertex_layout())
        .vertex_shader(shader_set.vertex_shader().main_entry_point(), ())
        .triangle_list()
        .viewports_dynamic_scissors_irrelevant(1)
        .viewports(std::iter::once(Viewport {
            origin: [0.0, 0.0],
            dimensions: [dimensions[0] as f32, dimensions[1] as f32],
            depth_range: 0.0 .. 1.0,
        }))
        .fragment_shader(shader_set.fragment_shader().main_entry_point(), ())
        .depth_stencil_simple_depth()
        .render_pass(Subpass::from(render_pass.clone(), 0).unwrap())
        .build(device.clone())?;

    Ok(Arc::new(pipeline))
}

/// Finds the best graphical device to render to.
fn rank_devices(devices: PhysicalDevicesIter) -> Result<PhysicalDevice, RendererCreationError> {
    devices.into_iter().map(|device|
        match device.ty() {
            PhysicalDeviceType::DiscreteGpu => (device, 4),
            PhysicalDeviceType::VirtualGpu => (device, 3),
            PhysicalDeviceType::IntegratedGpu => (device, 2),
            PhysicalDeviceType::Cpu => (device, 1),
            PhysicalDeviceType::Other => (device, 0),
        }
    ).max_by(|x, y| x.1.cmp(&y.1)).map(|(device, _)| device).ok_or(RendererCreationError::NoPhysicalDeviceError)
}

/// Returns current window dimensions.
pub fn get_window_dimensions(window: &Window) -> PhysicalSize {
    let dimensions = if let Some(dimensions) = window.get_inner_size() {
        dimensions.to_physical(window.get_hidpi_factor())
    } else {
        panic!("window was closed when calling get_window_dimensions");
    };

    dimensions
}

/// Returns current window dimensions.
pub fn get_window_dpi(window: &Window) -> f64 {
    window.get_hidpi_factor()
}

/// Creates new vulkan instance
fn create_new_instance() -> Result<Arc<Instance>, InstanceCreationError> {
    let extensions = vulkano_win::required_extensions();
    Instance::new(None, &extensions, None)
}

/// Creates new vulkan logical device
fn create_logical_device<'a>(physical_device: PhysicalDevice, physical_queues: &[(QueueFamily<'a>, f32)]) 
        -> Result<(Arc<Device>, QueuesIter), DeviceCreationError> {
    let minimal_features = vulkano::device::Features {
        depth_clamp: true, //needed for correct shadow mapping
        .. vulkano::device::Features::none()
    };

    let device_extensions_needed = vulkano::device::DeviceExtensions {
        khr_swapchain: true,
        .. vulkano::device::DeviceExtensions::none()
    };

    Device::new(
        physical_device, &minimal_features,
        &device_extensions_needed, physical_queues.iter().cloned()
    )
}

/// Creates a swapchain, which is a collection of images that are presented to the screen.
fn create_swapchain<'a>(surface: Arc<Surface<Window>>, physical_device: PhysicalDevice<'a>,
                        device: Arc<Device>, queues: &Queues) 
        -> Result<(Arc<Swapchain<Window>>, Vec<Arc<SwapchainImage<Window>>>), RendererCreationError> {
    let capabilities = surface.capabilities(physical_device)?;
    let usage = capabilities.supported_usage_flags;
    let format = capabilities.supported_formats[0].0;

    let initial_dimensions = match capabilities.current_extent {
        Some(dimensions) => dimensions,
        None => {
            let dimensions: (u32, u32) = get_window_dimensions(surface.window()).into();
            [dimensions.0, dimensions.1]
        }
    };

    let present_mode = {
        if capabilities.present_modes.mailbox {
            info!("Using Mailbox presentation mode");
            PresentMode::Mailbox
        } else {
            info!("Using Fifo presentation mode");
            PresentMode::Fifo
        }
    };

    Swapchain::new(
        device.clone(),
        surface.clone(),
        capabilities.min_image_count,
        format,
        initial_dimensions,
        1,
        usage,
        &queues.graphics_queue(),
        SurfaceTransform::Identity,
        CompositeAlpha::Opaque,
        present_mode,
        true,
        None
    ).map_err(RendererCreationError::from)
}

/// Creates render pass, which is a collection of attachments, subpasses, and dependencies between the subpasses.
fn create_renderpass(device: Arc<Device>, format: Format) -> Result<Arc<RenderPassAbstract + Send + Sync>, RenderPassCreationError> {
    let render_pass = single_pass_renderpass!(device.clone(),
                            attachments: {
                                color: {
                                    load: Clear,
                                    store: Store,
                                    format: format,
                                    samples: 1,
                                },
                                depth: {
                                    load: Clear,
                                    store: DontCare,
                                    format: Format::D16Unorm,
                                    samples: 1,
                                }
                            },
                            pass: {
                                color: [color],
                                depth_stencil: {depth}
                            }
                      )?;
    Ok(Arc::new(render_pass))
}