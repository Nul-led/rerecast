//! Contains methods for rasterizing triangles of a [`TrimeshedCollider`] into a [`Heightfield`].

use std::fmt::Display;

use bevy::math::{
    UVec3, Vec3A,
    bounding::{Aabb3d, IntersectsVolume as _},
};
use thiserror::Error;

use crate::{
    heightfield::{Heightfield, SpanInsertion, SpanInsertionError},
    span::{AreaType, Span, SpanBuilder},
    trimesh::TrimeshedCollider,
};

impl TrimeshedCollider {
    /// Marks the triangles as walkable or not based on the threshold angle.
    ///
    /// The triangles are marked as walkable if the normal angle is greater than the threshold angle.
    ///
    /// # Arguments
    ///
    /// * `threshold_rad` - The threshold angle in radians.
    ///
    pub fn mark_walkable_triangles(&mut self, threshold_rad: f32) {
        let threshold_cos = threshold_rad.cos();
        for (i, indices) in self.indices.iter().enumerate() {
            let normal = indices.normal(&self.vertices);

            if normal.y > threshold_cos {
                self.area_types[i] = AreaType::WALKABLE;
            }
        }
    }
}

impl Heightfield {
    /// Rasterizes the triangles of a [`TrimeshedCollider`] into a [`Heightfield`].
    ///
    /// # Arguments
    ///
    /// * `trimesh` - The [`TrimeshedCollider`] to rasterize.
    /// * `flag_merge_threshold` - The maximum difference between the ceilings of two spans to merge area type IDs.
    ///
    pub fn populate_from_trimesh(
        &mut self,
        trimesh: TrimeshedCollider,
        flag_merge_threshold: u32,
    ) -> Result<(), RasterizationError> {
        for (i, triangle) in trimesh.indices.iter().enumerate() {
            let triangle = [
                trimesh.vertices[triangle[0] as usize],
                trimesh.vertices[triangle[1] as usize],
                trimesh.vertices[triangle[2] as usize],
            ];
            let area_type = trimesh.area_types[i];
            self.rasterize_triangle(triangle, area_type, flag_merge_threshold)?;
        }
        Ok(())
    }

    fn rasterize_triangle(
        &mut self,
        triangle: [Vec3A; 3],
        area_type: AreaType,
        flag_merge_threshold: u32,
    ) -> Result<(), RasterizationError> {
        let aabb = triangle.aabb();
        // If the triangle does not touch the bounding box of the heightfield, skip the triangle.
        if !self.aabb.intersects(&aabb) {
            return Ok(());
        }

        let inverse_cell_size = 1.0 / self.cell_size;
        let inverse_cell_height = 1.0 / self.cell_height;

        let w = self.width as i32;
        let h = self.height as i32;
        // The height of the heightfield AABB
        let by = self.aabb.max[1] - self.aabb.min[1];

        // Calculate the footprint of the triangle on the grid's z-axis
        // z0 is the first z cell that the triangle touches
        // z1 is the last z cell that the triangle touches
        let z0 = ((aabb.min[2] - self.aabb.min[2]) * inverse_cell_size) as i32;
        let z1 = ((aabb.max[2] - self.aabb.min[2]) * inverse_cell_size) as i32;

        // use -1 rather than 0 to cut the polygon properly at the start of the tile
        let z0 = z0.clamp(-1, h - 1);
        let z1 = z1.clamp(0, h - 1);

        // Clip the triangle into all grid cells it touches.
        const MAX_VERTICES_AFTER_CLIPPING: usize = 7;
        // x-min, x-max, z-min, z-max
        const CLIP_DIRS: usize = 4;
        const BUF_LEN: usize = MAX_VERTICES_AFTER_CLIPPING * CLIP_DIRS;

        let mut buf = [Vec3A::ZERO; BUF_LEN];

        let (mut in_tri, rest) = buf.split_at_mut(MAX_VERTICES_AFTER_CLIPPING);
        let (mut in_row, rest) = rest.split_at_mut(MAX_VERTICES_AFTER_CLIPPING);
        let (mut p1, mut p2) = rest.split_at_mut(MAX_VERTICES_AFTER_CLIPPING);

        let mut nv_row = 0_u8;
        let mut nv_in = 3_u8;

        for z in z0..=z1 {
            // Clip polygon to row. Store the remaining polygon as well
            let cell_z = self.aabb.min[2] + z as f32 * self.cell_size;
            divide_poly(
                in_tri,
                nv_in,
                in_row,
                &mut nv_row,
                p1,
                &mut nv_in,
                cell_z + self.cell_size,
                DivisionAxis::Z,
            )?;
            std::mem::swap(&mut in_tri, &mut p1);

            if nv_row < 3 || z < 0 {
                continue;
            }

            // find X-axis bounds of the row
            let mut min_x = in_row[0].x;
            let mut max_x = in_row[0].x;
            for i in 1..nv_row {
                // No, max and min are *not* accidentally swapped here! :)
                min_x = min_x.max(in_row[i as usize].x);
                max_x = max_x.min(in_row[i as usize].x);
            }
            let x0 = ((min_x - self.aabb.min[0]) * inverse_cell_size) as i32;
            let x1 = ((max_x - self.aabb.min[0]) * inverse_cell_size) as i32;
            if x1 < 0 || x0 >= w {
                continue;
            }
            let x0 = x0.clamp(-1, w - 1);
            let x1 = x1.clamp(0, w - 1);

            let mut nv = 0_u8;
            let mut nv2 = nv_row;
            for x in x0..=x1 {
                // Clip polygon to column. store the remaining polygon as well
                let cx = self.aabb.min[0] + x as f32 * self.cell_size;
                divide_poly(
                    in_row,
                    nv2,
                    p1,
                    &mut nv,
                    p2,
                    &mut nv2,
                    cx + self.cell_size,
                    DivisionAxis::X,
                )?;
                std::mem::swap(&mut in_row, &mut p2);

                if nv < 3 || x < 0 {
                    continue;
                }

                // Calculate min and max of the span.
                let mut span_min = p1[1].y;
                let mut span_max = span_min;
                for i in 1..nv {
                    let y = p1[i as usize].y;
                    span_min = span_min.min(y);
                    span_max = span_max.max(y);
                }
                span_min -= self.aabb.min[1];
                span_max -= self.aabb.min[1];
                // Skip the span if it's completely outside the heightfield bounding box
                if span_max < 0.0 || span_min > by {
                    continue;
                }

                // Clamp the span to the heightfield bounding box.
                span_min = span_min.max(0.0);
                span_max = span_max.min(by);

                // Snap the span to the heightfield height grid.
                let span_min_cell_index =
                    ((span_min * inverse_cell_height).floor() as u16).clamp(0, Span::MAX_HEIGHT);
                let span_max_cell_index = ((span_max * inverse_cell_height).ceil() as u16)
                    .clamp(span_min_cell_index + 1, Span::MAX_HEIGHT);

                self.add_span(SpanInsertion {
                    x: x as u32,
                    z: z as u32,
                    span: SpanBuilder {
                        min: span_min_cell_index,
                        max: span_max_cell_index,
                        area: area_type,
                        next: None,
                    }
                    .build(),
                    flag_merge_threshold,
                })?;
            }
        }
        Ok(())
    }
}

/// Errors that can occur when rasterizing a triangle into a heightfield with [`Heightfield::populate_from_trimesh`].
#[derive(Error, Debug)]
pub enum RasterizationError {
    /// Happens when the polygon division fails.
    #[error("Failed to rasterize triangle: {0}")]
    PolygonDivisionError(#[from] PolygonDivisionError),
    /// Happens when the span insertion fails.
    #[error("Failed to add span: {0}")]
    SpanInsertionError(#[from] SpanInsertionError),
}

trait TriangleIndices {
    fn normal(&self, vertices: &[Vec3A]) -> Vec3A;
}

impl TriangleIndices for UVec3 {
    #[inline]
    fn normal(&self, vertices: &[Vec3A]) -> Vec3A {
        let a = vertices[self[0] as usize];
        let b = vertices[self[1] as usize];
        let c = vertices[self[2] as usize];
        let ab = b - a;
        let ac = c - a;
        ab.cross(ac).normalize_or_zero()
    }
}

trait TriangleVertices {
    fn aabb(&self) -> Aabb3d;
}

impl TriangleVertices for [Vec3A; 3] {
    #[inline]
    fn aabb(&self) -> Aabb3d {
        let min = self[0].min(self[1]).min(self[2]);
        let max = self[0].max(self[1]).max(self[2]);
        Aabb3d { min, max }
    }
}

/// Divides a convex polygon of max 12 vertices into two convex polygons
/// across a separating axis.
#[inline]
fn divide_poly(
    in_verts: &[Vec3A],
    in_vert_count: u8,
    out_verts_1: &mut [Vec3A],
    out_vert_count_1: &mut u8,
    out_verts_2: &mut [Vec3A],
    out_vert_count_2: &mut u8,
    axis_offset: f32,
    axis_dir: DivisionAxis,
) -> Result<(), PolygonDivisionError> {
    if in_vert_count > 12 {
        return Err(PolygonDivisionError::TooManyVertices(in_vert_count));
    } else if in_vert_count == 0 {
        return Err(PolygonDivisionError::NoVertices);
    }
    let in_vert_count = in_vert_count as usize;
    let axis_dir = axis_dir as usize;

    // How far positive or negative away from the separating axis is each vertex.
    let mut in_vert_axis_delta = [0.0; 12];
    for i in 0..in_vert_count {
        in_vert_axis_delta[i] = axis_offset - in_verts[i][axis_dir];
    }

    let mut poly_1_vert = 0;
    let mut poly_2_vert = 0;
    let mut in_vert_b = in_vert_count - 1;
    for in_vert_a in 0..in_vert_count {
        // If the two vertices are on the same side of the separating axis
        let same_side =
            (in_vert_axis_delta[in_vert_a] >= 0.0) == (in_vert_axis_delta[in_vert_b] >= 0.0);
        if !same_side {
            let s = in_vert_axis_delta[in_vert_b]
                / (in_vert_axis_delta[in_vert_b] - in_vert_axis_delta[in_vert_a]);
            out_verts_1[poly_1_vert] =
                in_verts[in_vert_b] + (in_verts[in_vert_a] - in_verts[in_vert_b]) * s;
            out_verts_2[poly_2_vert] = out_verts_1[poly_1_vert];

            poly_1_vert += 1;
            poly_2_vert += 1;

            // add the inVertA point to the right polygon. Do NOT add points that are on the dividing line
            // since these were already added above
            if in_vert_axis_delta[in_vert_a] > 0.0 {
                out_verts_1[poly_1_vert] = in_verts[in_vert_a];
                poly_1_vert += 1;
            } else if in_vert_axis_delta[in_vert_a] < 0.0 {
                out_verts_2[poly_2_vert] = in_verts[in_vert_a];
                poly_2_vert += 1;
            }
        } else {
            // add the inVertA point to the right polygon. Addition is done even for points on the dividing line
            if in_vert_axis_delta[in_vert_a] >= 0.0 {
                out_verts_1[poly_1_vert] = in_verts[in_vert_a];
                poly_1_vert += 1;
                if in_vert_axis_delta[in_vert_a] != 0.0 {
                    continue;
                }
            }
            out_verts_2[poly_2_vert] = in_verts[in_vert_a];
            poly_2_vert += 1;
        }

        in_vert_b = in_vert_a;
    }

    *out_vert_count_1 = poly_1_vert as u8;
    *out_vert_count_2 = poly_2_vert as u8;

    Ok(())
}

/// Errors that can occur when dividing a polygon into two convex polygons across a separating axis.
#[derive(Error, Debug)]
pub enum PolygonDivisionError {
    /// Happens when the polygon has more than 12 vertices.
    #[error("Failed to divide polygon: too many vertices. Expected at most 12, got {0}.")]
    TooManyVertices(u8),
    /// Happens when the polygon has no vertices.
    #[error("Failed to divide polygon: no vertices.")]
    NoVertices,
}

#[derive(Error, Debug)]
enum DivisionAxis {
    X = 0,
    Z = 2,
}

impl Display for DivisionAxis {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
