/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use api::{DeviceRect, FilterOp, MixBlendMode, PipelineId, PremultipliedColorF, PictureRect, PicturePoint};
use api::{DeviceIntRect, DeviceIntSize, DevicePoint, LayoutRect, PictureToRasterTransform};
use api::{DevicePixelScale, PictureIntPoint, PictureIntRect, PictureIntSize, RasterRect, RasterSpace};
use api::{PicturePixel, RasterPixel, WorldPixel};
use box_shadow::{BLUR_SAMPLE_SCALE};
use clip::ClipNodeCollector;
use clip_scroll_tree::{ROOT_SPATIAL_NODE_INDEX, ClipScrollTree, SpatialNodeIndex};
use euclid::{TypedScale, vec3};
use internal_types::PlaneSplitter;
use frame_builder::{FrameBuildingContext, FrameBuildingState, PictureState};
use frame_builder::{PictureContext, PrimitiveContext};
use gpu_cache::{GpuCacheAddress, GpuCacheHandle};
use gpu_types::{TransformPalette, TransformPaletteId, UvRectKind};
use plane_split::{Clipper, Polygon, Splitter};
use prim_store::{PictureIndex, PrimitiveInstance, SpaceMapper, PrimitiveDetails};
use prim_store::{Primitive, get_raster_rects, BrushKind, BrushPrimitive};
use render_task::{ClearMode, RenderTask, RenderTaskCacheEntryHandle};
use render_task::{RenderTaskCacheKey, RenderTaskCacheKeyKind, RenderTaskId, RenderTaskLocation};
use scene::{FilterOpHelpers, SceneProperties};
use smallvec::SmallVec;
use std::mem;
use tiling::RenderTargetKind;
use util::{TransformedRectKind, MatrixHelpers, MaxRect};

/*
 A picture represents a dynamically rendered image. It consists of:

 * A number of primitives that are drawn onto the picture.
 * A composite operation describing how to composite this
   picture into its parent.
 * A configuration describing how to draw the primitives on
   this picture (e.g. in screen space or local space).
 */

/// A context structure passed from parent to child
/// during the first frame building pass, when just
/// pictures are traversed.
pub struct PictureUpdateContext {
    pub surface_spatial_node_index: SpatialNodeIndex,
    pub raster_spatial_node_index: SpatialNodeIndex,
}

#[derive(Debug)]
pub struct RasterConfig {
    pub composite_mode: PictureCompositeMode,

    // If this picture is drawn to an intermediate surface,
    // the associated target information.
    pub surface: Option<PictureSurface>,

    // The spatial node of the rasterization root
    // for this picture.
    pub raster_spatial_node_index: SpatialNodeIndex,
}

/// Specifies how this Picture should be composited
/// onto the target it belongs to.
#[derive(Debug, Copy, Clone, PartialEq)]
pub enum PictureCompositeMode {
    /// Apply CSS mix-blend-mode effect.
    MixBlend(MixBlendMode),
    /// Apply a CSS filter.
    Filter(FilterOp),
    /// Draw to intermediate surface, copy straight across. This
    /// is used for CSS isolation, and plane splitting.
    Blit,
}

// Stores the location of the picture if it is drawn to
// an intermediate surface. This can be a render task if
// it is not persisted, or a texture cache item if the
// picture is cached in the texture cache.
#[derive(Debug)]
pub enum PictureSurface {
    RenderTask(RenderTaskId),
    TextureCache(RenderTaskCacheEntryHandle),
}

// A unique identifier for a Picture. Once we start
// doing deep compares of picture content, these
// may be the same across display lists, but that's
// not currently supported.
#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "capture", derive(Serialize))]
#[cfg_attr(feature = "replay", derive(Deserialize))]
pub struct PictureId(pub u64);

// TODO(gw): Having to generate globally unique picture
//           ids for caching is not ideal. We should be
//           able to completely remove this once we cache
//           pictures based on their content, rather than
//           the current cache key structure.
pub struct PictureIdGenerator {
    next: u64,
}

impl PictureIdGenerator {
    pub fn new() -> Self {
        PictureIdGenerator {
            next: 0,
        }
    }

    pub fn next(&mut self) -> PictureId {
        let id = PictureId(self.next);
        self.next += 1;
        id
    }
}

// Cache key that determines whether a pre-existing
// picture in the texture cache matches the content
// of the current picture.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
#[cfg_attr(feature = "capture", derive(Serialize))]
#[cfg_attr(feature = "replay", derive(Deserialize))]
pub struct PictureCacheKey {
    // NOTE: We specifically want to ensure that we
    //       don't include the device space origin
    //       of this picture in the cache key, because
    //       we want the cache to remain valid as it
    //       is scrolled and/or translated by animation.
    //       This is valid while we have the restriction
    //       in place that only pictures that use the
    //       root coordinate system are cached - once
    //       we relax that, we'll need to consider some
    //       extra parameters, depending on transform.

    // This is a globally unique id of the scene this picture
    // is associated with, to avoid picture id collisions.
    scene_id: u64,

    // The unique (for the scene_id) identifier for this picture.
    // TODO(gw): Currently, these will not be
    //           shared across new display lists,
    //           so will only remain valid during
    //           scrolling. Next step will be to
    //           allow deep comparisons on pictures
    //           between display lists, allowing
    //           pictures that are the same to be
    //           cached across display lists!
    picture_id: PictureId,

    // Store the rect within the unclipped device
    // rect that we are actually rendering. This ensures
    // that if the 'clipped' rect changes, we will see
    // that the cache is invalid and re-draw the picture.
    // TODO(gw): To reduce the number of invalidations that
    //           happen as a cached picture scrolls off-screen,
    //           we could round up the size of the off-screen
    //           targets we draw (e.g. 512 pixels). This may
    //           also simplify other parts of the code that
    //           deal with clipped/unclipped rects, such as
    //           the code to inflate the device rect for blurs.
    pic_relative_render_rect: PictureIntRect,

    // Ensure that if the overall size of the picture
    // changes, the cache key will not match. This can
    // happen, for example, during zooming or changes
    // in device-pixel-ratio.
    unclipped_size: DeviceIntSize,
}

/// Enum value describing the place of a picture in a 3D context.
#[derive(Clone, Debug)]
pub enum Picture3DContext<C> {
    /// The picture is not a part of 3D context sub-hierarchy.
    Out,
    /// The picture is a part of 3D context.
    In {
        /// Additional data per child for the case of this a root of 3D hierarchy.
        root_data: Option<Vec<C>>,
        /// The spatial node index of an "ancestor" element, i.e. one
        /// that establishes the transformed element’s containing block.
        ///
        /// See CSS spec draft for more details:
        /// https://drafts.csswg.org/css-transforms-2/#accumulated-3d-transformation-matrix-computation
        ancestor_index: SpatialNodeIndex,
    },
}

/// Information about a preserve-3D hierarchy child that has been plane-split
/// and ordered according to the view direction.
#[derive(Clone, Debug)]
pub struct OrderedPictureChild {
    pub anchor: usize,
    pub transform_id: TransformPaletteId,
    pub gpu_address: GpuCacheAddress,
}

/// A list of primitive instances that are added to a picture
/// This ensures we can keep a list of primitives that
/// are pictures, for a fast initial traversal of the picture
/// tree without walking the instance list.
pub struct PrimitiveList {
    pub prim_instances: Vec<PrimitiveInstance>,
    pub pictures: SmallVec<[PictureIndex; 4]>,
}

impl PrimitiveList {
    pub fn new(
        prim_instances: Vec<PrimitiveInstance>,
        primitives: &[Primitive],
    ) -> Self {
        let mut pictures = SmallVec::new();

        // Walk the list of primitive instances and extract any that
        // are pictures.
        for prim_instance in &prim_instances {
            let prim = &primitives[prim_instance.prim_index.0];
            match prim.details {
                PrimitiveDetails::Brush(BrushPrimitive { kind: BrushKind::Picture { pic_index }, .. }) => {
                    pictures.push(pic_index);
                }
                _ => {
                    // TODO(gw): For now, we don't do anything with non-picture primitives.
                    //           However, in the near future we'll accumulate the static
                    //           bounding rects for primitive clusters here, so that we can
                    //           quickly determine the size of a picture during frame building.
                }
            }
        }

        PrimitiveList {
            prim_instances,
            pictures,
        }
    }
}

pub struct PicturePrimitive {
    // List of primitive runs that make up this picture.
    pub prim_instances: Vec<PrimitiveInstance>,
    pub state: Option<(PictureState, PictureContext)>,

    // The pipeline that the primitives on this picture belong to.
    pub pipeline_id: PipelineId,

    // If true, apply the local clip rect to primitive drawn
    // in this picture.
    pub apply_local_clip_rect: bool,

    // If a mix-blend-mode, contains the render task for
    // the readback of the framebuffer that we use to sample
    // from in the mix-blend-mode shader.
    // For drop-shadow filter, this will store the original
    // picture task which would be rendered on screen after
    // blur pass.
    pub secondary_render_task_id: Option<RenderTaskId>,
    /// How this picture should be composited.
    /// If None, don't composite - just draw directly on parent surface.
    pub requested_composite_mode: Option<PictureCompositeMode>,
    /// Requested rasterization space for this picture. It is
    /// a performance hint only.
    pub requested_raster_space: RasterSpace,

    pub raster_config: Option<RasterConfig>,
    pub context_3d: Picture3DContext<OrderedPictureChild>,

    // If requested as a frame output (for rendering
    // pages to a texture), this is the pipeline this
    // picture is the root of.
    pub frame_output_pipeline_id: Option<PipelineId>,
    // An optional cache handle for storing extra data
    // in the GPU cache, depending on the type of
    // picture.
    pub extra_gpu_data_handle: GpuCacheHandle,

    // Unique identifier for this picture.
    pub id: PictureId,

    /// List of the children that are picture primitives.
    child_pictures: SmallVec<[PictureIndex; 4]>,

    /// The spatial node index of this picture when it is
    /// composited into the parent picture.
    spatial_node_index: SpatialNodeIndex,
}

impl PicturePrimitive {
    fn resolve_scene_properties(&mut self, properties: &SceneProperties) -> bool {
        match self.requested_composite_mode {
            Some(PictureCompositeMode::Filter(ref mut filter)) => {
                match *filter {
                    FilterOp::Opacity(ref binding, ref mut value) => {
                        *value = properties.resolve_float(binding);
                    }
                    _ => {}
                }

                filter.is_visible()
            }
            _ => true,
        }
    }

    fn is_visible(&self) -> bool {
        match self.requested_composite_mode {
            Some(PictureCompositeMode::Filter(ref filter)) => {
                filter.is_visible()
            }
            _ => true,
        }
    }

    pub fn new_image(
        id: PictureId,
        requested_composite_mode: Option<PictureCompositeMode>,
        context_3d: Picture3DContext<OrderedPictureChild>,
        pipeline_id: PipelineId,
        frame_output_pipeline_id: Option<PipelineId>,
        apply_local_clip_rect: bool,
        requested_raster_space: RasterSpace,
        prim_list: PrimitiveList,
        spatial_node_index: SpatialNodeIndex,
    ) -> Self {
        PicturePrimitive {
            prim_instances: prim_list.prim_instances,
            child_pictures: prim_list.pictures,
            state: None,
            secondary_render_task_id: None,
            requested_composite_mode,
            raster_config: None,
            context_3d,
            frame_output_pipeline_id,
            extra_gpu_data_handle: GpuCacheHandle::new(),
            apply_local_clip_rect,
            pipeline_id,
            id,
            requested_raster_space,
            spatial_node_index,
        }
    }

    pub fn take_context(
        &mut self,
        pic_index: PictureIndex,
        prim_context: &PrimitiveContext,
        parent_spatial_node_index: SpatialNodeIndex,
        surface_spatial_node_index: SpatialNodeIndex,
        raster_spatial_node_index: SpatialNodeIndex,
        parent_allows_subpixel_aa: bool,
        frame_state: &mut FrameBuildingState,
        frame_context: &FrameBuildingContext,
        is_chased: bool,
    ) -> Option<(PictureContext, PictureState, Vec<PrimitiveInstance>)> {
        if !self.is_visible() {
            if cfg!(debug_assertions) && is_chased {
                println!("\tculled for carrying an invisible composite filter");
            }

            return None;
        }

        // Extract the raster and surface spatial nodes from the raster
        // config, if this picture establishes a surface. Otherwise just
        // pass in the spatial node indices from the parent context.
        let (raster_spatial_node_index, surface_spatial_node_index) = match self.raster_config {
            Some(ref raster_config) => {
                (raster_config.raster_spatial_node_index, self.spatial_node_index)
            }
            None => {
                (raster_spatial_node_index, surface_spatial_node_index)
            }
        };

        if self.raster_config.is_some() {
            frame_state.clip_store
                .push_surface(surface_spatial_node_index);
        }

        let map_pic_to_world = SpaceMapper::new_with_target(
            ROOT_SPATIAL_NODE_INDEX,
            surface_spatial_node_index,
            frame_context.world_rect,
            frame_context.clip_scroll_tree,
        );

        let pic_bounds = map_pic_to_world.unmap(&map_pic_to_world.bounds)
                                         .unwrap_or(PictureRect::max_rect());

        let map_local_to_pic = SpaceMapper::new(
            surface_spatial_node_index,
            pic_bounds,
        );

        let (map_raster_to_world, map_pic_to_raster) = create_raster_mappers(
            surface_spatial_node_index,
            raster_spatial_node_index,
            frame_context,
        );

        let (containing_block_index, plane_splitter) = match self.context_3d {
            Picture3DContext::Out => {
                (parent_spatial_node_index, None)
            }
            Picture3DContext::In { root_data: Some(_), ancestor_index } => {
                (ancestor_index, Some(PlaneSplitter::new()))
            }
            Picture3DContext::In { root_data: None, ancestor_index } => {
                (ancestor_index, None)
            }
        };

        let map_local_to_containing_block = SpaceMapper::new(
            containing_block_index,
            LayoutRect::zero(), // bounds aren't going to be used for this mapping
        );

        let state = PictureState {
            tasks: Vec::new(),
            has_non_root_coord_system: false,
            is_cacheable: true,
            local_rect_changed: false,
            rect: PictureRect::zero(),
            map_local_to_pic,
            map_pic_to_world,
            map_pic_to_raster,
            map_raster_to_world,
            map_local_to_containing_block,
            plane_splitter,
        };

        // Disallow subpixel AA if an intermediate surface is needed.
        // TODO(lsalzman): allow overriding parent if intermediate surface is opaque
        let allow_subpixel_aa = parent_allows_subpixel_aa &&
            self.raster_config.is_none();

        let inflation_factor = match self.raster_config {
            Some(RasterConfig { composite_mode: PictureCompositeMode::Filter(FilterOp::Blur(blur_radius)), .. }) => {
                // The amount of extra space needed for primitives inside
                // this picture to ensure the visibility check is correct.
                BLUR_SAMPLE_SCALE * blur_radius
            }
            _ => {
                0.0
            }
        };

        let context = PictureContext {
            pic_index,
            pipeline_id: self.pipeline_id,
            apply_local_clip_rect: self.apply_local_clip_rect,
            inflation_factor,
            allow_subpixel_aa,
            is_passthrough: self.raster_config.is_none(),
            raster_space: self.requested_raster_space,
            local_spatial_node_index: prim_context.spatial_node_index,
            raster_spatial_node_index,
            surface_spatial_node_index,
        };

        let instances = mem::replace(&mut self.prim_instances, Vec::new());

        Some((context, state, instances))
    }

    pub fn restore_context(
        &mut self,
        prim_instances: Vec<PrimitiveInstance>,
        context: PictureContext,
        state: PictureState,
        local_rect: Option<PictureRect>,
        frame_state: &mut FrameBuildingState,
    ) -> (LayoutRect, Option<ClipNodeCollector>) {
        let local_rect = match local_rect {
            Some(local_rect) => {
                let local_content_rect = LayoutRect::from_untyped(&local_rect.to_untyped());

                match self.raster_config {
                    Some(RasterConfig { composite_mode: PictureCompositeMode::Filter(FilterOp::Blur(blur_radius)), .. }) => {
                        let inflate_size = (blur_radius * BLUR_SAMPLE_SCALE).ceil();
                        local_content_rect.inflate(inflate_size, inflate_size)
                    }
                    Some(RasterConfig { composite_mode: PictureCompositeMode::Filter(FilterOp::DropShadow(_, blur_radius, _)), .. }) => {
                        let inflate_size = (blur_radius * BLUR_SAMPLE_SCALE).ceil();
                        local_content_rect.inflate(inflate_size, inflate_size)

                        // TODO(gw): When we support culling rect being separate from
                        //           the task/screen rect, we should include both the
                        //           content and shadow rect here, which will prevent
                        //           drop-shadows from disappearing if the main content
                        //           rect is not visible. Something like:
                        // let shadow_rect = local_content_rect
                        //     .inflate(inflate_size, inflate_size)
                        //     .translate(&offset);
                        // shadow_rect.union(&local_content_rect)
                    }
                    _ => {
                        local_content_rect
                    }
                }
            }
            None => {
                assert!(self.raster_config.is_none());
                LayoutRect::zero()
            }
        };

        let clip_node_collector = if context.is_passthrough {
            None
        } else {
            Some(frame_state.clip_store.pop_surface())
        };

        self.prim_instances = prim_instances;
        self.state = Some((state, context));

        (local_rect, clip_node_collector)
    }

    pub fn take_state_and_context(&mut self) -> (PictureState, PictureContext) {
        self.state.take().expect("bug: no state present!")
    }

    /// Add a primitive instance to the plane splitter. The function would generate
    /// an appropriate polygon, clip it against the frustum, and register with the
    /// given plane splitter.
    pub fn add_split_plane(
        splitter: &mut PlaneSplitter,
        transforms: &TransformPalette,
        prim_instance: &PrimitiveInstance,
        original_local_rect: LayoutRect,
        plane_split_anchor: usize,
    ) -> bool {
        let transform = transforms
            .get_world_transform(prim_instance.spatial_node_index);
        let matrix = transform.cast();

        // Apply the local clip rect here, before splitting. This is
        // because the local clip rect can't be applied in the vertex
        // shader for split composites, since we are drawing polygons
        // rather that rectangles. The interpolation still works correctly
        // since we determine the UVs by doing a bilerp with a factor
        // from the original local rect.
        let local_rect = match original_local_rect
            .intersection(&prim_instance.combined_local_clip_rect)
        {
            Some(rect) => rect.cast(),
            None => return false,
        };

        match transform.transform_kind() {
            TransformedRectKind::AxisAligned => {
                let inv_transform = transforms
                    .get_world_inv_transform(prim_instance.spatial_node_index);
                let polygon = Polygon::from_transformed_rect_with_inverse(
                    local_rect,
                    &matrix,
                    &inv_transform.cast(),
                    plane_split_anchor,
                ).unwrap();
                splitter.add(polygon);
            }
            TransformedRectKind::Complex => {
                let mut clipper = Clipper::new();
                let results = clipper.clip_transformed(
                    Polygon::from_rect(
                        local_rect,
                        plane_split_anchor,
                    ),
                    &matrix,
                    prim_instance.clipped_world_rect.map(|r| r.to_f64()),
                );
                if let Ok(results) = results {
                    for poly in results {
                        splitter.add(poly);
                    }
                }
            }
        }

        true
    }

    pub fn resolve_split_planes(
        &mut self,
        splitter: &mut PlaneSplitter,
        frame_state: &mut FrameBuildingState,
        clip_scroll_tree: &ClipScrollTree,
    ) {
        let ordered = match self.context_3d {
            Picture3DContext::In { root_data: Some(ref mut list), .. } => list,
            _ => panic!("Expected to find 3D context root"),
        };
        ordered.clear();

        // Process the accumulated split planes and order them for rendering.
        // Z axis is directed at the screen, `sort` is ascending, and we need back-to-front order.
        for poly in splitter.sort(vec3(0.0, 0.0, 1.0)) {
            let spatial_node_index = self.prim_instances[poly.anchor].spatial_node_index;

            let transform = frame_state.transforms.get_world_inv_transform(spatial_node_index);
            let transform_id = frame_state.transforms.get_id(
                spatial_node_index,
                ROOT_SPATIAL_NODE_INDEX,
                clip_scroll_tree,
            );

            let local_points = [
                transform.transform_point3d(&poly.points[0].cast()).unwrap(),
                transform.transform_point3d(&poly.points[1].cast()).unwrap(),
                transform.transform_point3d(&poly.points[2].cast()).unwrap(),
                transform.transform_point3d(&poly.points[3].cast()).unwrap(),
            ];
            let gpu_blocks = [
                [local_points[0].x, local_points[0].y, local_points[1].x, local_points[1].y].into(),
                [local_points[2].x, local_points[2].y, local_points[3].x, local_points[3].y].into(),
            ];
            let gpu_handle = frame_state.gpu_cache.push_per_frame_blocks(&gpu_blocks);
            let gpu_address = frame_state.gpu_cache.get_address(&gpu_handle);

            ordered.push(OrderedPictureChild {
                anchor: poly.anchor,
                transform_id,
                gpu_address,
            });
        }
    }

    /// Called during initial picture traversal, before we know the
    /// bounding rect of children. It is possible to determine the
    /// surface / raster config now though.
    pub fn pre_update(
        &mut self,
        context: &PictureUpdateContext,
        frame_context: &FrameBuildingContext,
    ) -> Option<(PictureUpdateContext, SmallVec<[PictureIndex; 4]>)> {
        // Reset raster config in case we early out below.
        self.raster_config = None;

        if !self.resolve_scene_properties(frame_context.scene_properties) {
            return None;
        }

        let actual_composite_mode = match self.requested_composite_mode {
            Some(PictureCompositeMode::Filter(filter)) if filter.is_noop() => None,
            mode => mode,
        };

        let has_surface = actual_composite_mode.is_some();

        let surface_spatial_node_index = if has_surface {
            self.spatial_node_index
        } else {
            context.surface_spatial_node_index
        };

        let xf = frame_context.clip_scroll_tree.get_relative_transform(
            context.raster_spatial_node_index,
            surface_spatial_node_index,
        ).expect("BUG: unable to get relative transform");

        // TODO(gw): A temporary hack here to revert behavior to
        //           always raster in screen-space. This is not
        //           a problem yet, since we're not taking advantage
        //           of this for caching yet. This is a workaround
        //           for some existing issues with handling scale
        //           when rasterizing in local space mode. Once
        //           the fixes for those are in-place, we can
        //           remove this hack!
        //let local_scale = raster_space.local_scale();
        // let wants_raster_root = xf.has_perspective_component() ||
        //                         local_scale.is_some();
        let wants_raster_root = xf.has_perspective_component();

        let establishes_raster_root = has_surface && wants_raster_root;

        let raster_spatial_node_index = if establishes_raster_root {
            surface_spatial_node_index
        } else {
            context.raster_spatial_node_index
        };

        self.raster_config = actual_composite_mode.map(|composite_mode| {
            RasterConfig {
                composite_mode,
                surface: None,
                raster_spatial_node_index,
            }
        });

        let child_context = PictureUpdateContext {
            raster_spatial_node_index,
            surface_spatial_node_index,
        };

        Some((child_context, mem::replace(&mut self.child_pictures, SmallVec::new())))
    }

    /// Called after updating child pictures during the initial
    /// picture traversal.
    pub fn post_update(
        &mut self,
        child_pictures: SmallVec<[PictureIndex; 4]>,
    ) {
        self.child_pictures = child_pictures;
    }

    pub fn prepare_for_render(
        &mut self,
        pic_index: PictureIndex,
        prim_instance: &PrimitiveInstance,
        prim_local_rect: &LayoutRect,
        pic_state: &mut PictureState,
        frame_context: &FrameBuildingContext,
        frame_state: &mut FrameBuildingState,
    ) -> bool {
        let (mut pic_state_for_children, pic_context) = self.take_state_and_context();

        if let Some(ref mut splitter) = pic_state_for_children.plane_splitter {
            self.resolve_split_planes(
                splitter,
                frame_state,
                frame_context.clip_scroll_tree,
            );
        }

        let raster_config = match self.raster_config {
            Some(ref mut raster_config) => raster_config,
            None => {
                pic_state.tasks.extend(pic_state_for_children.tasks);
                return true
            }
        };

        let (map_raster_to_world, map_pic_to_raster) = create_raster_mappers(
            prim_instance.spatial_node_index,
            raster_config.raster_spatial_node_index,
            frame_context,
        );

        let pic_rect = PictureRect::from_untyped(&prim_local_rect.to_untyped());

        let (clipped, unclipped, transform) = match get_raster_rects(
            pic_rect,
            &map_pic_to_raster,
            &map_raster_to_world,
            prim_instance.clipped_world_rect.expect("bug1"),
            frame_context.device_pixel_scale,
        ) {
            Some(info) => info,
            None => return false,
        };

        // TODO(gw): Almost all of the Picture types below use extra_gpu_cache_data
        //           to store the same type of data. The exception is the filter
        //           with a ColorMatrix, which stores the color matrix here. It's
        //           probably worth tidying this code up to be a bit more consistent.
        //           Perhaps store the color matrix after the common data, even though
        //           it's not used by that shader.

        match raster_config.composite_mode {
            PictureCompositeMode::Filter(FilterOp::Blur(blur_radius)) => {
                let blur_std_deviation = blur_radius * frame_context.device_pixel_scale.0;
                let blur_range = (blur_std_deviation * BLUR_SAMPLE_SCALE).ceil() as i32;

                // The clipped field is the part of the picture that is visible
                // on screen. The unclipped field is the screen-space rect of
                // the complete picture, if no screen / clip-chain was applied
                // (this includes the extra space for blur region). To ensure
                // that we draw a large enough part of the picture to get correct
                // blur results, inflate that clipped area by the blur range, and
                // then intersect with the total screen rect, to minimize the
                // allocation size.
                let device_rect = clipped
                    .inflate(blur_range, blur_range)
                    .intersection(&unclipped.to_i32())
                    .unwrap();

                let uv_rect_kind = calculate_uv_rect_kind(
                    &pic_rect,
                    &transform,
                    &device_rect,
                    frame_context.device_pixel_scale,
                );

                // If we are drawing a blur that has primitives or clips that contain
                // a complex coordinate system, don't bother caching them (for now).
                // It's likely that they are animating and caching may not help here
                // anyway. In the future we should relax this a bit, so that we can
                // cache tasks with complex coordinate systems if we detect the
                // relevant transforms haven't changed from frame to frame.
                let surface = if pic_state_for_children.has_non_root_coord_system ||
                                 !pic_state_for_children.is_cacheable {
                    let picture_task = RenderTask::new_picture(
                        RenderTaskLocation::Dynamic(None, device_rect.size),
                        unclipped.size,
                        pic_index,
                        device_rect.origin,
                        pic_state_for_children.tasks,
                        uv_rect_kind,
                        pic_context.raster_spatial_node_index,
                    );

                    let picture_task_id = frame_state.render_tasks.add(picture_task);

                    let blur_render_task = RenderTask::new_blur(
                        blur_std_deviation,
                        picture_task_id,
                        frame_state.render_tasks,
                        RenderTargetKind::Color,
                        ClearMode::Transparent,
                    );

                    let render_task_id = frame_state.render_tasks.add(blur_render_task);

                    pic_state.tasks.push(render_task_id);

                    PictureSurface::RenderTask(render_task_id)
                } else {
                    // Get the relative clipped rect within the overall prim rect, that
                    // forms part of the cache key.
                    let pic_relative_render_rect = PictureIntRect::new(
                        PictureIntPoint::new(
                            device_rect.origin.x - unclipped.origin.x as i32,
                            device_rect.origin.y - unclipped.origin.y as i32,
                        ),
                        PictureIntSize::new(
                            device_rect.size.width,
                            device_rect.size.height,
                        ),
                    );

                    // Request a render task that will cache the output in the
                    // texture cache.
                    let cache_item = frame_state.resource_cache.request_render_task(
                        RenderTaskCacheKey {
                            size: device_rect.size,
                            kind: RenderTaskCacheKeyKind::Picture(PictureCacheKey {
                                scene_id: frame_context.scene_id,
                                picture_id: self.id,
                                unclipped_size: unclipped.size.to_i32(),
                                pic_relative_render_rect,
                            }),
                        },
                        frame_state.gpu_cache,
                        frame_state.render_tasks,
                        None,
                        false,
                        |render_tasks| {
                            let child_tasks = mem::replace(&mut pic_state_for_children.tasks, Vec::new());

                            let picture_task = RenderTask::new_picture(
                                RenderTaskLocation::Dynamic(None, device_rect.size),
                                unclipped.size,
                                pic_index,
                                device_rect.origin,
                                child_tasks,
                                uv_rect_kind,
                                pic_context.raster_spatial_node_index,
                            );

                            let picture_task_id = render_tasks.add(picture_task);

                            let blur_render_task = RenderTask::new_blur(
                                blur_std_deviation,
                                picture_task_id,
                                render_tasks,
                                RenderTargetKind::Color,
                                ClearMode::Transparent,
                            );

                            let render_task_id = render_tasks.add(blur_render_task);

                            pic_state.tasks.push(render_task_id);

                            render_task_id
                        }
                    );

                    PictureSurface::TextureCache(cache_item)
                };

                raster_config.surface = Some(surface);
            }
            PictureCompositeMode::Filter(FilterOp::DropShadow(offset, blur_radius, color)) => {
                let blur_std_deviation = blur_radius * frame_context.device_pixel_scale.0;
                let blur_range = (blur_std_deviation * BLUR_SAMPLE_SCALE).ceil() as i32;

                // The clipped field is the part of the picture that is visible
                // on screen. The unclipped field is the screen-space rect of
                // the complete picture, if no screen / clip-chain was applied
                // (this includes the extra space for blur region). To ensure
                // that we draw a large enough part of the picture to get correct
                // blur results, inflate that clipped area by the blur range, and
                // then intersect with the total screen rect, to minimize the
                // allocation size.
                let device_rect = clipped
                    .inflate(blur_range, blur_range)
                    .intersection(&unclipped.to_i32())
                    .unwrap();

                let uv_rect_kind = calculate_uv_rect_kind(
                    &pic_rect,
                    &transform,
                    &device_rect,
                    frame_context.device_pixel_scale,
                );

                let mut picture_task = RenderTask::new_picture(
                    RenderTaskLocation::Dynamic(None, device_rect.size),
                    unclipped.size,
                    pic_index,
                    device_rect.origin,
                    pic_state_for_children.tasks,
                    uv_rect_kind,
                    pic_context.raster_spatial_node_index,
                );
                picture_task.mark_for_saving();

                let picture_task_id = frame_state.render_tasks.add(picture_task);

                let blur_render_task = RenderTask::new_blur(
                    blur_std_deviation.round(),
                    picture_task_id,
                    frame_state.render_tasks,
                    RenderTargetKind::Color,
                    ClearMode::Transparent,
                );

                self.secondary_render_task_id = Some(picture_task_id);

                let render_task_id = frame_state.render_tasks.add(blur_render_task);
                pic_state.tasks.push(render_task_id);
                raster_config.surface = Some(PictureSurface::RenderTask(render_task_id));

                // If the local rect of the contents changed, force the cache handle
                // to be invalidated so that the primitive data below will get
                // uploaded to the GPU this frame. This can occur during property
                // animation.
                if pic_state.local_rect_changed {
                    frame_state.gpu_cache.invalidate(&mut self.extra_gpu_data_handle);
                }

                if let Some(mut request) = frame_state.gpu_cache.request(&mut self.extra_gpu_data_handle) {
                    // TODO(gw): This is very hacky code below! It stores an extra
                    //           brush primitive below for the special case of a
                    //           drop-shadow where we need a different local
                    //           rect for the shadow. To tidy this up in future,
                    //           we could consider abstracting the code in prim_store.rs
                    //           that writes a brush primitive header.

                    // Basic brush primitive header is (see end of prepare_prim_for_render_inner in prim_store.rs)
                    //  [brush specific data]
                    //  [segment_rect, segment data]
                    let shadow_rect = prim_local_rect.translate(&offset);

                    // ImageBrush colors
                    request.push(color.premultiplied());
                    request.push(PremultipliedColorF::WHITE);
                    request.push([
                        prim_local_rect.size.width,
                        prim_local_rect.size.height,
                        0.0,
                        0.0,
                    ]);

                    // segment rect / extra data
                    request.push(shadow_rect);
                    request.push([0.0, 0.0, 0.0, 0.0]);
                }
            }
            PictureCompositeMode::MixBlend(..) => {
                let uv_rect_kind = calculate_uv_rect_kind(
                    &pic_rect,
                    &transform,
                    &clipped,
                    frame_context.device_pixel_scale,
                );

                let picture_task = RenderTask::new_picture(
                    RenderTaskLocation::Dynamic(None, clipped.size),
                    unclipped.size,
                    pic_index,
                    clipped.origin,
                    pic_state_for_children.tasks,
                    uv_rect_kind,
                    pic_context.raster_spatial_node_index,
                );

                let readback_task_id = frame_state.render_tasks.add(
                    RenderTask::new_readback(clipped)
                );

                self.secondary_render_task_id = Some(readback_task_id);
                pic_state.tasks.push(readback_task_id);

                let render_task_id = frame_state.render_tasks.add(picture_task);
                pic_state.tasks.push(render_task_id);
                raster_config.surface = Some(PictureSurface::RenderTask(render_task_id));
            }
            PictureCompositeMode::Filter(filter) => {
                if let FilterOp::ColorMatrix(m) = filter {
                    if let Some(mut request) = frame_state.gpu_cache.request(&mut self.extra_gpu_data_handle) {
                        for i in 0..5 {
                            request.push([m[i*4], m[i*4+1], m[i*4+2], m[i*4+3]]);
                        }
                    }
                }

                let uv_rect_kind = calculate_uv_rect_kind(
                    &pic_rect,
                    &transform,
                    &clipped,
                    frame_context.device_pixel_scale,
                );

                let picture_task = RenderTask::new_picture(
                    RenderTaskLocation::Dynamic(None, clipped.size),
                    unclipped.size,
                    pic_index,
                    clipped.origin,
                    pic_state_for_children.tasks,
                    uv_rect_kind,
                    pic_context.raster_spatial_node_index,
                );

                let render_task_id = frame_state.render_tasks.add(picture_task);
                pic_state.tasks.push(render_task_id);
                raster_config.surface = Some(PictureSurface::RenderTask(render_task_id));
            }
            PictureCompositeMode::Blit => {
                let uv_rect_kind = calculate_uv_rect_kind(
                    &pic_rect,
                    &transform,
                    &clipped,
                    frame_context.device_pixel_scale,
                );

                let picture_task = RenderTask::new_picture(
                    RenderTaskLocation::Dynamic(None, clipped.size),
                    unclipped.size,
                    pic_index,
                    clipped.origin,
                    pic_state_for_children.tasks,
                    uv_rect_kind,
                    pic_context.raster_spatial_node_index,
                );

                let render_task_id = frame_state.render_tasks.add(picture_task);
                pic_state.tasks.push(render_task_id);
                raster_config.surface = Some(PictureSurface::RenderTask(render_task_id));
            }
        }

        true
    }
}

// Calculate a single screen-space UV for a picture.
fn calculate_screen_uv(
    local_pos: &PicturePoint,
    transform: &PictureToRasterTransform,
    rendered_rect: &DeviceRect,
    device_pixel_scale: DevicePixelScale,
) -> DevicePoint {
    let raster_pos = match transform.transform_point2d(local_pos) {
        Some(pos) => pos,
        None => {
            //Warning: this is incorrect and needs to be fixed properly.
            // The transformation has put a local vertex behind the near clipping plane...
            // Proper solution would be to keep the near-clipping-plane results around
            // (currently produced by calculate_screen_bounding_rect) and use them here.
            return DevicePoint::new(0.5, 0.5);
        }
    };

    let raster_to_device_space = TypedScale::new(1.0) * device_pixel_scale;

    let mut device_pos = raster_pos * raster_to_device_space;

    // Apply snapping for axis-aligned scroll nodes, as per prim_shared.glsl.
    if transform.transform_kind() == TransformedRectKind::AxisAligned {
        device_pos.x = (device_pos.x + 0.5).floor();
        device_pos.y = (device_pos.y + 0.5).floor();
    }

    DevicePoint::new(
        (device_pos.x - rendered_rect.origin.x) / rendered_rect.size.width,
        (device_pos.y - rendered_rect.origin.y) / rendered_rect.size.height,
    )
}

// Calculate a UV rect within an image based on the screen space
// vertex positions of a picture.
fn calculate_uv_rect_kind(
    pic_rect: &PictureRect,
    transform: &PictureToRasterTransform,
    rendered_rect: &DeviceIntRect,
    device_pixel_scale: DevicePixelScale,
) -> UvRectKind {
    let rendered_rect = rendered_rect.to_f32();

    let top_left = calculate_screen_uv(
        &pic_rect.origin,
        transform,
        &rendered_rect,
        device_pixel_scale,
    );

    let top_right = calculate_screen_uv(
        &pic_rect.top_right(),
        transform,
        &rendered_rect,
        device_pixel_scale,
    );

    let bottom_left = calculate_screen_uv(
        &pic_rect.bottom_left(),
        transform,
        &rendered_rect,
        device_pixel_scale,
    );

    let bottom_right = calculate_screen_uv(
        &pic_rect.bottom_right(),
        transform,
        &rendered_rect,
        device_pixel_scale,
    );

    UvRectKind::Quad {
        top_left,
        top_right,
        bottom_left,
        bottom_right,
    }
}

fn create_raster_mappers(
    surface_spatial_node_index: SpatialNodeIndex,
    raster_spatial_node_index: SpatialNodeIndex,
    frame_context: &FrameBuildingContext,
) -> (SpaceMapper<RasterPixel, WorldPixel>, SpaceMapper<PicturePixel, RasterPixel>) {
    let map_raster_to_world = SpaceMapper::new_with_target(
        ROOT_SPATIAL_NODE_INDEX,
        raster_spatial_node_index,
        frame_context.world_rect,
        frame_context.clip_scroll_tree,
    );

    let raster_bounds = map_raster_to_world.unmap(&frame_context.world_rect)
                                           .unwrap_or(RasterRect::max_rect());

    let map_pic_to_raster = SpaceMapper::new_with_target(
        raster_spatial_node_index,
        surface_spatial_node_index,
        raster_bounds,
        frame_context.clip_scroll_tree,
    );

    (map_raster_to_world, map_pic_to_raster)
}
