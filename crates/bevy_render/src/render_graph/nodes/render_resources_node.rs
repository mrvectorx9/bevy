use crate::{
    draw::{Draw, RenderPipelines},
    render_graph::{CommandQueue, Node, ResourceSlots, SystemNode},
    render_resource::{
        self, BufferInfo, BufferUsage, RenderResourceAssignment, RenderResourceAssignments,
        RenderResourceAssignmentsId, RenderResourceHints, RenderResourceId,
    },
    renderer::{RenderContext, RenderResourceContext, RenderResources},
    texture,
};

use bevy_asset::{Assets, Handle};
use legion::prelude::*;
use render_resource::ResourceInfo;
use std::{collections::HashMap, marker::PhantomData};

pub const BIND_BUFFER_ALIGNMENT: usize = 256;
#[derive(Debug)]
struct QueuedBufferWrite {
    buffer: RenderResourceId,
    target_offset: usize,
    source_offset: usize,
    size: usize,
}

#[derive(Debug)]
struct BufferArrayStatus {
    changed_item_count: usize,
    item_size: usize,
    aligned_size: usize,
    staging_buffer_offset: usize,
    buffer: Option<RenderResourceId>,
    queued_buffer_writes: Vec<QueuedBufferWrite>,
    current_item_count: usize,
    current_item_capacity: usize,
    indices: HashMap<RenderResourceAssignmentsId, usize>,
    current_index: usize,
    // TODO: this is a hack to workaround RenderResources without a fixed length
    changed_size: usize,
    current_offset: usize,
}

impl BufferArrayStatus {
    pub fn get_or_assign_index(&mut self, id: RenderResourceAssignmentsId) -> usize {
        if let Some(offset) = self.indices.get(&id) {
            *offset
        } else {
            if self.current_index == self.current_item_capacity {
                panic!("no empty slots available in array");
            }

            let index = self.current_index;
            self.indices.insert(id, index);
            self.current_index += 1;
            index
        }
    }
}

struct UniformBufferArrays<T>
where
    T: render_resource::RenderResources,
{
    uniform_arrays: Vec<Option<(String, BufferArrayStatus)>>,
    _marker: PhantomData<T>,
}

impl<T> Default for UniformBufferArrays<T>
where
    T: render_resource::RenderResources,
{
    fn default() -> Self {
        Self {
            uniform_arrays: Default::default(),
            _marker: Default::default(),
        }
    }
}

impl<T> UniformBufferArrays<T>
where
    T: render_resource::RenderResources,
{
    fn reset_changed_item_counts(&mut self) {
        for buffer_status in self.uniform_arrays.iter_mut() {
            if let Some((_name, buffer_status)) = buffer_status {
                buffer_status.changed_item_count = 0;
                buffer_status.current_offset = 0;
                buffer_status.changed_size = 0;
            }
        }
    }

    fn increment_changed_item_counts(&mut self, uniforms: &T) {
        if self.uniform_arrays.len() != uniforms.render_resources_len() {
            self.uniform_arrays
                .resize_with(uniforms.render_resources_len(), || None);
        }
        for (i, render_resource) in uniforms.iter_render_resources().enumerate() {
            if let Some(ResourceInfo::Buffer(_)) = render_resource.resource_info() {
                let render_resource_name = uniforms.get_render_resource_name(i).unwrap();
                let size = render_resource.buffer_byte_len().unwrap();
                if let Some((ref _name, ref mut buffer_array_status)) = self.uniform_arrays[i] {
                    buffer_array_status.changed_item_count += 1;
                    buffer_array_status.changed_size += size;
                } else {
                    self.uniform_arrays[i] = Some((
                        render_resource_name.to_string(),
                        BufferArrayStatus {
                            changed_item_count: 1,
                            queued_buffer_writes: Vec::new(),
                            aligned_size: Self::get_aligned_dynamic_uniform_size(size),
                            item_size: size,
                            staging_buffer_offset: 0,
                            buffer: None,
                            current_index: 0,
                            current_item_count: 0,
                            current_item_capacity: 0,
                            indices: HashMap::new(),
                            changed_size: size,
                            current_offset: 0,
                        },
                    ))
                }
            }
        }
    }

    fn get_aligned_dynamic_uniform_size(data_size: usize) -> usize {
        BIND_BUFFER_ALIGNMENT * ((data_size as f32 / BIND_BUFFER_ALIGNMENT as f32).ceil() as usize)
    }

    fn setup_buffer_arrays(
        &mut self,
        render_resource_context: &dyn RenderResourceContext,
        dynamic_uniforms: bool,
    ) {
        for buffer_array_status in self.uniform_arrays.iter_mut() {
            if let Some((_name, buffer_array_status)) = buffer_array_status {
                if dynamic_uniforms {
                    Self::setup_buffer_array(buffer_array_status, render_resource_context, true);
                }

                buffer_array_status.queued_buffer_writes =
                    Vec::with_capacity(buffer_array_status.changed_item_count);
            }
        }
    }

    fn setup_buffer_array(
        buffer_array_status: &mut BufferArrayStatus,
        render_resource_context: &dyn RenderResourceContext,
        align: bool,
    ) {
        if buffer_array_status.current_item_capacity < buffer_array_status.changed_item_count {
            let new_capacity =
                buffer_array_status.changed_item_count + buffer_array_status.changed_item_count / 2;
            let mut item_size = buffer_array_status.item_size;
            if align {
                item_size = Self::get_aligned_dynamic_uniform_size(item_size);
            }

            let total_size = item_size * new_capacity;

            let buffer = render_resource_context.create_buffer(BufferInfo {
                size: total_size,
                buffer_usage: BufferUsage::COPY_DST | BufferUsage::UNIFORM,
            });

            buffer_array_status.current_item_capacity = new_capacity;

            log::trace!(
                "creating buffer for uniform {}. size: {} item_capacity: {} item_size: {}",
                std::any::type_name::<T>(),
                total_size,
                new_capacity,
                item_size
            );

            buffer_array_status.buffer = Some(buffer);
        }
    }
    fn update_staging_buffer_offsets(&mut self) -> usize {
        let mut size = 0;
        for dynamic_buffer_array_status in self.uniform_arrays.iter_mut() {
            if let Some((_name, ref mut buffer_array_status)) = dynamic_buffer_array_status {
                buffer_array_status.staging_buffer_offset = size;
                size += buffer_array_status.changed_size;
            }
        }

        size
    }

    fn setup_uniform_buffer_resources(
        &mut self,
        uniforms: &T,
        dynamic_uniforms: bool,
        render_resources: &dyn RenderResourceContext,
        render_resource_assignments: &mut RenderResourceAssignments,
        staging_buffer: &mut [u8],
    ) {
        for (i, render_resource) in uniforms.iter_render_resources().enumerate() {
            match render_resource.resource_info() {
                Some(ResourceInfo::Buffer(_)) => {
                    let size = render_resource.buffer_byte_len().unwrap();
                    let render_resource_name = uniforms.get_render_resource_name(i).unwrap();
                    let (_name, uniform_buffer_status) = self.uniform_arrays[i].as_mut().unwrap();
                    let range = 0..size as u64;
                    let (target_buffer, target_offset) = if dynamic_uniforms {
                        let buffer = uniform_buffer_status.buffer.unwrap();
                        let index = uniform_buffer_status
                            .get_or_assign_index(render_resource_assignments.id);
                        render_resource_assignments.set(
                            render_resource_name,
                            RenderResourceAssignment::Buffer {
                                resource: buffer,
                                dynamic_index: Some(
                                    (index * uniform_buffer_status.aligned_size) as u32,
                                ),
                                range,
                            },
                        );
                        (buffer, index * uniform_buffer_status.aligned_size)
                    } else {
                        let mut matching_buffer = None;
                        let mut buffer_to_remove = None;
                        if let Some(assignment) =
                            render_resource_assignments.get(render_resource_name)
                        {
                            let resource = assignment.get_resource();
                            render_resources.get_resource_info(resource, &mut |info| {
                                if let Some(ResourceInfo::Buffer(Some(BufferInfo {
                                    size: current_size,
                                    ..
                                }))) = info
                                {
                                    if size == *current_size {
                                        matching_buffer = Some(resource);
                                    } else {
                                        // TODO: if get_resource_info returns a type instead of taking a closure, move buffer free here
                                        buffer_to_remove = Some(resource);
                                    }
                                }
                            })
                        }

                        if let Some(buffer) = buffer_to_remove {
                            render_resources.remove_buffer(buffer);
                        }

                        let resource = if let Some(matching_buffer) = matching_buffer {
                            matching_buffer
                        } else {
                            let mut usage = BufferUsage::UNIFORM;
                            if let Some(render_resource_hints) =
                                uniforms.get_render_resource_hints(i)
                            {
                                if render_resource_hints.contains(RenderResourceHints::BUFFER) {
                                    usage = BufferUsage::STORAGE
                                }
                            }

                            let resource = render_resources.create_buffer(BufferInfo {
                                size,
                                buffer_usage: BufferUsage::COPY_DST | usage,
                                ..Default::default()
                            });

                            render_resource_assignments.set(
                                render_resource_name,
                                RenderResourceAssignment::Buffer {
                                    resource,
                                    range,
                                    dynamic_index: None,
                                },
                            );
                            resource
                        };

                        (resource, 0)
                    };

                    let staging_buffer_start = uniform_buffer_status.staging_buffer_offset
                        + uniform_buffer_status.current_offset;

                    render_resource.write_buffer_bytes(
                        &mut staging_buffer[staging_buffer_start..(staging_buffer_start + size)],
                    );

                    uniform_buffer_status
                        .queued_buffer_writes
                        .push(QueuedBufferWrite {
                            buffer: target_buffer,
                            target_offset,
                            source_offset: uniform_buffer_status.current_offset,
                            size,
                        });
                    uniform_buffer_status.current_offset += size;
                }
                Some(ResourceInfo::Texture(_)) => { /* ignore textures */ }
                Some(ResourceInfo::Sampler) => { /* ignore samplers */ }
                None => { /* ignore None */ }
            }
        }
    }

    fn copy_staging_buffer_to_final_buffers(
        &mut self,
        command_queue: &mut CommandQueue,
        staging_buffer: RenderResourceId,
    ) {
        for uniform_buffer_status in self.uniform_arrays.iter_mut() {
            if let Some((_name, buffer_array_status)) = uniform_buffer_status {
                let start = buffer_array_status.staging_buffer_offset;
                for queued_buffer_write in buffer_array_status.queued_buffer_writes.drain(..) {
                    command_queue.copy_buffer_to_buffer(
                        staging_buffer,
                        (start + queued_buffer_write.source_offset) as u64,
                        queued_buffer_write.buffer,
                        queued_buffer_write.target_offset as u64,
                        queued_buffer_write.size as u64,
                    )
                }
            }
        }
    }
}

#[derive(Default)]
pub struct RenderResourcesNode<T>
where
    T: render_resource::RenderResources,
{
    command_queue: CommandQueue,
    dynamic_uniforms: bool,
    _marker: PhantomData<T>,
}

impl<T> RenderResourcesNode<T>
where
    T: render_resource::RenderResources,
{
    pub fn new(dynamic_uniforms: bool) -> Self {
        RenderResourcesNode {
            command_queue: CommandQueue::default(),
            dynamic_uniforms,
            _marker: PhantomData::default(),
        }
    }
}

impl<T> Node for RenderResourcesNode<T>
where
    T: render_resource::RenderResources,
{
    fn update(
        &mut self,
        _world: &World,
        _resources: &Resources,
        render_context: &mut dyn RenderContext,
        _input: &ResourceSlots,
        _output: &mut ResourceSlots,
    ) {
        self.command_queue.execute(render_context);
    }
}

impl<T> SystemNode for RenderResourcesNode<T>
where
    T: render_resource::RenderResources,
{
    fn get_system(&self) -> Box<dyn Schedulable> {
        let mut command_queue = self.command_queue.clone();
        let mut uniform_buffer_arrays = UniformBufferArrays::<T>::default();
        let dynamic_uniforms = self.dynamic_uniforms;
        // TODO: maybe run "update" here
        (move |world: &mut SubWorld,
               render_resources: Res<RenderResources>,
               query: &mut Query<(Read<T>, Read<Draw>, Write<RenderPipelines>)>| {
            let render_resource_context = &*render_resources.context;

            uniform_buffer_arrays.reset_changed_item_counts();
            // update uniforms info
            for (uniforms, draw, _render_pipelines) in query.iter_mut(world) {
                if !draw.is_visible {
                    return;
                }

                uniform_buffer_arrays.increment_changed_item_counts(&uniforms);
            }

            uniform_buffer_arrays.setup_buffer_arrays(render_resource_context, dynamic_uniforms);
            let staging_buffer_size = uniform_buffer_arrays.update_staging_buffer_offsets();

            for (uniforms, draw, mut render_pipelines) in query.iter_mut(world) {
                if !draw.is_visible {
                    return;
                }

                setup_uniform_texture_resources::<T>(
                    &uniforms,
                    render_resource_context,
                    &mut render_pipelines.render_resource_assignments,
                )
            }

            if staging_buffer_size == 0 {
                let mut staging_buffer: [u8; 0] = [];
                for (uniforms, draw, mut render_pipelines) in query.iter_mut(world) {
                    if !draw.is_visible {
                        return;
                    }

                    uniform_buffer_arrays.setup_uniform_buffer_resources(
                        &uniforms,
                        dynamic_uniforms,
                        render_resource_context,
                        &mut render_pipelines.render_resource_assignments,
                        &mut staging_buffer,
                    );
                }
            } else {
                let staging_buffer = render_resource_context.create_buffer_mapped(
                    BufferInfo {
                        buffer_usage: BufferUsage::COPY_SRC,
                        size: staging_buffer_size,
                        ..Default::default()
                    },
                    &mut |mut staging_buffer, _render_resources| {
                        for (uniforms, draw, mut render_pipelines) in query.iter_mut(world) {
                            if !draw.is_visible {
                                return;
                            }

                            uniform_buffer_arrays.setup_uniform_buffer_resources(
                                &uniforms,
                                dynamic_uniforms,
                                render_resource_context,
                                &mut render_pipelines.render_resource_assignments,
                                &mut staging_buffer,
                            );
                        }
                    },
                );

                uniform_buffer_arrays
                    .copy_staging_buffer_to_final_buffers(&mut command_queue, staging_buffer);
                command_queue.free_buffer(staging_buffer);
            }
        })
        .system()
    }
}

#[derive(Default)]
pub struct AssetRenderResourcesNode<T>
where
    T: render_resource::RenderResources,
{
    command_queue: CommandQueue,
    dynamic_uniforms: bool,
    _marker: PhantomData<T>,
}

impl<T> AssetRenderResourcesNode<T>
where
    T: render_resource::RenderResources,
{
    pub fn new(dynamic_uniforms: bool) -> Self {
        AssetRenderResourcesNode {
            dynamic_uniforms,
            command_queue: Default::default(),
            _marker: Default::default(),
        }
    }
}

impl<T> Node for AssetRenderResourcesNode<T>
where
    T: render_resource::RenderResources,
{
    fn update(
        &mut self,
        _world: &World,
        _resources: &Resources,
        render_context: &mut dyn RenderContext,
        _input: &ResourceSlots,
        _output: &mut ResourceSlots,
    ) {
        self.command_queue.execute(render_context);
    }
}

const EXPECT_ASSET_MESSAGE: &str = "Only assets that exist should be in the modified assets list";

impl<T> SystemNode for AssetRenderResourcesNode<T>
where
    T: render_resource::RenderResources,
{
    fn get_system(&self) -> Box<dyn Schedulable> {
        let mut command_queue = self.command_queue.clone();
        let mut uniform_buffer_arrays = UniformBufferArrays::<T>::default();
        // let mut asset_event_reader = EventReader::<AssetEvent<T>>::default();
        let mut asset_render_resource_assignments =
            HashMap::<Handle<T>, RenderResourceAssignments>::default();
        let dynamic_uniforms = self.dynamic_uniforms;
        (move |world: &mut SubWorld,
               assets: Res<Assets<T>>,
               //    asset_events: Res<Events<AssetEvent<T>>>,
               render_resources: Res<RenderResources>,
               query: &mut Query<(Read<Handle<T>>, Read<Draw>, Write<RenderPipelines>)>| {
            let render_resource_context = &*render_resources.context;
            uniform_buffer_arrays.reset_changed_item_counts();

            let modified_assets = assets
                .iter()
                .map(|(handle, _)| handle)
                .collect::<Vec<Handle<T>>>();
            // TODO: uncomment this when asset dependency events are added https://github.com/bevyengine/bevy/issues/26
            // let mut modified_assets = HashSet::new();
            // for event in asset_event_reader.iter(&asset_events) {
            //     match event {
            //         AssetEvent::Created { handle } => {
            //             modified_assets.insert(*handle);
            //         }
            //         AssetEvent::Modified { handle } => {
            //             modified_assets.insert(*handle);
            //         }
            //         AssetEvent::Removed { handle } => {
            //             // TODO: handle removals
            //             modified_assets.remove(handle);
            //         }
            //     }
            // }

            // update uniform handles info
            for asset_handle in modified_assets.iter() {
                let asset = assets.get(&asset_handle).expect(EXPECT_ASSET_MESSAGE);
                uniform_buffer_arrays.increment_changed_item_counts(&asset);
            }

            uniform_buffer_arrays.setup_buffer_arrays(render_resource_context, dynamic_uniforms);
            let staging_buffer_size = uniform_buffer_arrays.update_staging_buffer_offsets();

            for asset_handle in modified_assets.iter() {
                let asset = assets.get(&asset_handle).expect(EXPECT_ASSET_MESSAGE);
                let mut render_resource_assignments = asset_render_resource_assignments
                    .entry(*asset_handle)
                    .or_insert_with(|| RenderResourceAssignments::default());
                setup_uniform_texture_resources::<T>(
                    &asset,
                    render_resource_context,
                    &mut render_resource_assignments,
                );
            }

            if staging_buffer_size == 0 {
                let mut staging_buffer: [u8; 0] = [];
                for asset_handle in modified_assets.iter() {
                    let asset = assets.get(&asset_handle).expect(EXPECT_ASSET_MESSAGE);
                    let mut render_resource_assignments = asset_render_resource_assignments
                        .entry(*asset_handle)
                        .or_insert_with(|| RenderResourceAssignments::default());
                    // TODO: only setup buffer if we haven't seen this handle before
                    uniform_buffer_arrays.setup_uniform_buffer_resources(
                        &asset,
                        dynamic_uniforms,
                        render_resource_context,
                        &mut render_resource_assignments,
                        &mut staging_buffer,
                    );
                }
            } else {
                let staging_buffer = render_resource_context.create_buffer_mapped(
                    BufferInfo {
                        buffer_usage: BufferUsage::COPY_SRC,
                        size: staging_buffer_size,
                        ..Default::default()
                    },
                    &mut |mut staging_buffer, _render_resources| {
                        for asset_handle in modified_assets.iter() {
                            let asset = assets.get(&asset_handle).expect(EXPECT_ASSET_MESSAGE);
                            let mut render_resource_assignments = asset_render_resource_assignments
                                .entry(*asset_handle)
                                .or_insert_with(|| RenderResourceAssignments::default());
                            // TODO: only setup buffer if we haven't seen this handle before
                            uniform_buffer_arrays.setup_uniform_buffer_resources(
                                &asset,
                                dynamic_uniforms,
                                render_resource_context,
                                &mut render_resource_assignments,
                                &mut staging_buffer,
                            );
                        }
                    },
                );

                uniform_buffer_arrays
                    .copy_staging_buffer_to_final_buffers(&mut command_queue, staging_buffer);
                command_queue.free_buffer(staging_buffer);
            }

            for (asset_handle, _draw, mut render_pipelines) in query.iter_mut(world) {
                if let Some(asset_assignments) =
                    asset_render_resource_assignments.get(&asset_handle)
                {
                    render_pipelines
                        .render_resource_assignments
                        .extend(asset_assignments);
                }
            }
        })
        .system()
    }
}

fn setup_uniform_texture_resources<T>(
    uniforms: &T,
    render_resource_context: &dyn RenderResourceContext,
    render_resource_assignments: &mut RenderResourceAssignments,
) where
    T: render_resource::RenderResources,
{
    for (i, render_resource) in uniforms.iter_render_resources().enumerate() {
        if let Some(ResourceInfo::Texture(_)) = render_resource.resource_info() {
            let render_resource_name = uniforms.get_render_resource_name(i).unwrap();
            let sampler_name = format!("{}_sampler", render_resource_name);
            if let Some(texture_handle) = render_resource.texture() {
                if let Some(texture_resource) = render_resource_context
                    .get_asset_resource(texture_handle, texture::TEXTURE_ASSET_INDEX)
                {
                    let sampler_resource = render_resource_context
                        .get_asset_resource(texture_handle, texture::SAMPLER_ASSET_INDEX)
                        .unwrap();

                    render_resource_assignments.set(
                        render_resource_name,
                        RenderResourceAssignment::Texture(texture_resource),
                    );
                    render_resource_assignments.set(
                        &sampler_name,
                        RenderResourceAssignment::Sampler(sampler_resource),
                    );
                    continue;
                }
            }
        }
    }
}