//! A procedurally built two-stage gearbox: enough real geometry, with crisp
//! feature edges and meshing gears, to stand in for a CAD assembly.

use bytemuck::{Pod, Zeroable};
use glam::{Mat4, Vec3};
use std::f32::consts::{FRAC_PI_2, PI, TAU};

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Vertex {
    pub pos: [f32; 3],
    pub normal: [f32; 3],
}

#[derive(Clone, Copy, Debug)]
pub struct MeshRange {
    pub first_index: u32,
    pub index_count: u32,
    pub base_vertex: i32,
    pub first_edge: u32,
    pub edge_count: u32,
    pub min: Vec3,
    pub max: Vec3,
}

#[derive(Clone, Debug)]
pub struct Part {
    pub name: String,
    pub mesh: usize,
    /// Linear RGB.
    pub color: [f32; 3],
    pub base: Mat4,
    /// Rotation about the part's local Z, in radians per second of model time.
    pub spin: f32,
    pub phase: f32,
}

impl Part {
    pub fn transform(&self, time: f32) -> Mat4 {
        self.base * Mat4::from_rotation_z(self.phase + self.spin * time)
    }
}

pub struct Model {
    pub vertices: Vec<Vertex>,
    pub indices: Vec<u32>,
    pub edge_vertices: Vec<[f32; 3]>,
    pub meshes: Vec<MeshRange>,
    pub parts: Vec<Part>,
    pub min: Vec3,
    pub max: Vec3,
}

impl Model {
    pub fn triangles_per_assembly(&self) -> u64 {
        self.parts
            .iter()
            .map(|part| u64::from(self.meshes[part.mesh].index_count / 3))
            .sum()
    }

    pub fn part_triangles(&self, part: usize) -> u32 {
        self.meshes[self.parts[part].mesh].index_count / 3
    }
}

#[derive(Default)]
struct MeshBuilder {
    vertices: Vec<Vertex>,
    indices: Vec<u32>,
    edges: Vec<[f32; 3]>,
}

impl MeshBuilder {
    fn vertex(&mut self, pos: Vec3, normal: Vec3) -> u32 {
        let index = self.vertices.len() as u32;
        self.vertices.push(Vertex {
            pos: pos.to_array(),
            normal: normal.to_array(),
        });
        index
    }

    /// `a b c d` counter-clockwise seen from the side the normals face.
    fn quad(&mut self, a: u32, b: u32, c: u32, d: u32) {
        self.indices.extend_from_slice(&[a, b, c, a, c, d]);
    }

    fn edge(&mut self, a: Vec3, b: Vec3) {
        self.edges.push(a.to_array());
        self.edges.push(b.to_array());
    }

    /// Revolves a `(radius, z)` polyline about Z. Going along the profile,
    /// the solid lies to the left, as in `(0,z0) (r,z0) (r,z1) (0,z1)`. Every
    /// profile vertex is a crease and draws a circle of feature edges.
    fn revolve(&mut self, profile: &[(f32, f32)], segments: u32) {
        let around = |r: f32, z: f32, angle: f32| Vec3::new(r * angle.cos(), r * angle.sin(), z);
        for pair in profile.windows(2) {
            let ((r0, z0), (r1, z1)) = (pair[0], pair[1]);
            let (dr, dz) = (r1 - r0, z1 - z0);
            let length = (dr * dr + dz * dz).sqrt();
            if length < 1e-5 {
                continue;
            }
            let (normal_r, normal_z) = (dz / length, -dr / length);
            let first = self.vertices.len() as u32;
            for segment in 0..=segments {
                let angle = TAU * segment as f32 / segments as f32;
                let normal = Vec3::new(normal_r * angle.cos(), normal_r * angle.sin(), normal_z);
                self.vertex(around(r0, z0, angle), normal);
                self.vertex(around(r1, z1, angle), normal);
            }
            for segment in 0..segments {
                let at = first + segment * 2;
                if r0 < 1e-4 {
                    self.indices.extend_from_slice(&[at, at + 3, at + 1]);
                } else if r1 < 1e-4 {
                    self.indices.extend_from_slice(&[at, at + 2, at + 1]);
                } else {
                    self.quad(at, at + 2, at + 3, at + 1);
                }
            }
        }
        let closed = profile.first() == profile.last();
        let circles = if closed { &profile[1..] } else { profile };
        for &(r, z) in circles {
            if r < 1e-4 {
                continue;
            }
            for segment in 0..segments {
                let a0 = TAU * segment as f32 / segments as f32;
                let a1 = TAU * (segment + 1) as f32 / segments as f32;
                self.edge(around(r, z, a0), around(r, z, a1));
            }
        }
    }

    /// Extrudes the band between a counter-clockwise outline and a bore of
    /// `bore` radius, from `z0` to `z1`. `sharp[i]` marks outline corners:
    /// they get flat shading and a vertical feature edge.
    fn extrude_ring(&mut self, outline: &[Vec3], sharp: &[bool], bore: f32, z0: f32, z1: f32) {
        let count = outline.len();
        let next = |index: usize| (index + 1) % count;
        let previous = |index: usize| (index + count - 1) % count;
        let at = |point: Vec3, z: f32| Vec3::new(point.x, point.y, z);
        let inner: Vec<Vec3> = outline
            .iter()
            .map(|point| Vec3::new(point.x, point.y, 0.0).normalize() * bore)
            .collect();

        // Top and bottom faces.
        for index in 0..count {
            let j = next(index);
            let a = self.vertex(at(inner[index], z1), Vec3::Z);
            let b = self.vertex(at(outline[index], z1), Vec3::Z);
            let c = self.vertex(at(outline[j], z1), Vec3::Z);
            let d = self.vertex(at(inner[j], z1), Vec3::Z);
            self.quad(a, b, c, d);
            let a = self.vertex(at(inner[j], z0), -Vec3::Z);
            let b = self.vertex(at(outline[j], z0), -Vec3::Z);
            let c = self.vertex(at(outline[index], z0), -Vec3::Z);
            let d = self.vertex(at(inner[index], z0), -Vec3::Z);
            self.quad(a, b, c, d);
        }

        // The outer wall, smooth along the flanks and creased at corners.
        let segment_normal = |index: usize| {
            let delta = outline[next(index)] - outline[index];
            Vec3::new(delta.y, -delta.x, 0.0).normalize()
        };
        for index in 0..count {
            let j = next(index);
            let this = segment_normal(index);
            let start = if sharp[index] {
                this
            } else {
                (segment_normal(previous(index)) + this).normalize()
            };
            let end = if sharp[j] {
                this
            } else {
                (this + segment_normal(j)).normalize()
            };
            let a = self.vertex(at(outline[index], z0), start);
            let b = self.vertex(at(outline[j], z0), end);
            let c = self.vertex(at(outline[j], z1), end);
            let d = self.vertex(at(outline[index], z1), start);
            self.quad(a, b, c, d);
        }

        // The bore, whose normals face the axis.
        for index in 0..count {
            let j = next(index);
            let (near, far) = (-inner[index].normalize(), -inner[j].normalize());
            let a = self.vertex(at(inner[j], z0), far);
            let b = self.vertex(at(inner[index], z0), near);
            let c = self.vertex(at(inner[index], z1), near);
            let d = self.vertex(at(inner[j], z1), far);
            self.quad(a, b, c, d);
        }

        for index in 0..count {
            let j = next(index);
            for z in [z0, z1] {
                self.edge(at(outline[index], z), at(outline[j], z));
                self.edge(at(inner[index], z), at(inner[j], z));
            }
            if sharp[index] {
                self.edge(at(outline[index], z0), at(outline[index], z1));
            }
        }
    }

    /// A solid prism over a convex counter-clockwise polygon.
    fn prism(&mut self, polygon: &[Vec3], z0: f32, z1: f32) {
        let at = |point: Vec3, z: f32| Vec3::new(point.x, point.y, z);
        let count = polygon.len();
        let top = self.vertex(Vec3::new(0.0, 0.0, z1), Vec3::Z);
        let bottom = self.vertex(Vec3::new(0.0, 0.0, z0), -Vec3::Z);
        for index in 0..count {
            let j = (index + 1) % count;
            let a = self.vertex(at(polygon[index], z1), Vec3::Z);
            let b = self.vertex(at(polygon[j], z1), Vec3::Z);
            self.indices.extend_from_slice(&[top, a, b]);
            let a = self.vertex(at(polygon[j], z0), -Vec3::Z);
            let b = self.vertex(at(polygon[index], z0), -Vec3::Z);
            self.indices.extend_from_slice(&[bottom, a, b]);

            let delta = polygon[j] - polygon[index];
            let normal = Vec3::new(delta.y, -delta.x, 0.0).normalize();
            let a = self.vertex(at(polygon[index], z0), normal);
            let b = self.vertex(at(polygon[j], z0), normal);
            let c = self.vertex(at(polygon[j], z1), normal);
            let d = self.vertex(at(polygon[index], z1), normal);
            self.quad(a, b, c, d);

            self.edge(at(polygon[index], z0), at(polygon[j], z0));
            self.edge(at(polygon[index], z1), at(polygon[j], z1));
            self.edge(at(polygon[index], z0), at(polygon[index], z1));
        }
    }

    fn cuboid(&mut self, min: Vec3, max: Vec3) {
        let corner = |x: bool, y: bool, z: bool| {
            Vec3::new(
                if x { max.x } else { min.x },
                if y { max.y } else { min.y },
                if z { max.z } else { min.z },
            )
        };
        let faces: [(Vec3, [(bool, bool, bool); 4]); 6] = [
            (Vec3::X, [(true, false, false), (true, true, false), (true, true, true), (true, false, true)]),
            (-Vec3::X, [(false, true, false), (false, false, false), (false, false, true), (false, true, true)]),
            (Vec3::Y, [(true, true, false), (false, true, false), (false, true, true), (true, true, true)]),
            (-Vec3::Y, [(false, false, false), (true, false, false), (true, false, true), (false, false, true)]),
            (Vec3::Z, [(false, false, true), (true, false, true), (true, true, true), (false, true, true)]),
            (-Vec3::Z, [(false, true, false), (true, true, false), (true, false, false), (false, false, false)]),
        ];
        for (normal, corners) in faces {
            let positions = corners.map(|(x, y, z)| corner(x, y, z));
            let a = self.vertex(positions[0], normal);
            let b = self.vertex(positions[1], normal);
            let c = self.vertex(positions[2], normal);
            let d = self.vertex(positions[3], normal);
            self.quad(a, b, c, d);
            for index in 0..4 {
                self.edge(positions[index], positions[(index + 1) % 4]);
            }
        }
    }
}

/// A gear's outline, with a tooth centered on angle 0, and which of its
/// points are corners.
fn gear_outline(teeth: u32, module: f32) -> (Vec<Vec3>, Vec<bool>) {
    const ROOT_ARC: u32 = 3;
    const FLANK: u32 = 5;
    const TIP_ARC: u32 = 3;
    let pitch_radius = module * teeth as f32 * 0.5;
    let (tip, root) = (pitch_radius + module, pitch_radius - 1.25 * module);
    let pitch = TAU / teeth as f32;
    let (half_root, half_pitch, half_tip) = (pitch * 0.34, pitch * 0.25, pitch * 0.13);
    // Half the tooth's angular width across the flank, curved through the
    // pitch circle: a close enough stand-in for an involute at this scale.
    let flank_half = |s: f32| {
        let at_pitch = (pitch_radius - root) / (tip - root);
        let (l0, l1, l2) = (
            (s - at_pitch) * (s - 1.0) / (at_pitch * 1.0),
            s * (s - 1.0) / (at_pitch * (at_pitch - 1.0)),
            s * (s - at_pitch) / (1.0 - at_pitch),
        );
        half_root * l0 + half_pitch * l1 + half_tip * l2
    };
    let polar = |radius: f32, angle: f32| Vec3::new(radius * angle.cos(), radius * angle.sin(), 0.0);

    let mut points = Vec::new();
    let mut sharp = Vec::new();
    for tooth in 0..teeth {
        let center = tooth as f32 * pitch;
        // The root, from the gap's middle up to the tooth.
        for step in 0..ROOT_ARC {
            let s = step as f32 / ROOT_ARC as f32;
            let angle = center - pitch * 0.5 + (pitch * 0.5 - half_root) * s;
            points.push(polar(root, angle));
            sharp.push(false);
        }
        // Up the near flank.
        for step in 0..FLANK {
            let s = step as f32 / FLANK as f32;
            points.push(polar(root + (tip - root) * s, center - flank_half(s)));
            sharp.push(step == 0);
        }
        // Across the tip.
        for step in 0..=TIP_ARC {
            let s = step as f32 / TIP_ARC as f32;
            points.push(polar(tip, center - half_tip + 2.0 * half_tip * s));
            sharp.push(step == 0 || step == TIP_ARC);
        }
        // Down the far flank.
        for step in (0..FLANK).rev() {
            let s = step as f32 / FLANK as f32;
            points.push(polar(root + (tip - root) * s, center + flank_half(s)));
            sharp.push(step == 0);
        }
        // The root again, on to the next gap's middle.
        for step in 1..ROOT_ARC {
            let s = step as f32 / ROOT_ARC as f32;
            let angle = center + half_root + (pitch * 0.5 - half_root) * s;
            points.push(polar(root, angle));
            sharp.push(false);
        }
    }
    (points, sharp)
}

fn srgb(hex: u32) -> [f32; 3] {
    let channel = |value: u32| {
        let value = (value & 0xff) as f32 / 255.0;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    [channel(hex >> 16), channel(hex >> 8), channel(hex)]
}

/// The phase a driven gear needs for its teeth to mesh with its driver's.
/// `alpha` is the direction from the driver's center to the driven one's.
fn mesh_phase(driver_phase: f32, driver_teeth: u32, driven_teeth: u32, alpha: f32) -> f32 {
    alpha + PI + PI / driven_teeth as f32
        - (driver_phase - alpha) * driver_teeth as f32 / driven_teeth as f32
}

pub fn gearbox() -> Model {
    const MODULE: f32 = 4.0;
    const SEGMENTS: u32 = 96;

    let mut model = Model {
        vertices: Vec::new(),
        indices: Vec::new(),
        edge_vertices: Vec::new(),
        meshes: Vec::new(),
        parts: Vec::new(),
        min: Vec3::splat(f32::MAX),
        max: Vec3::splat(f32::MIN),
    };
    let add_mesh = |model: &mut Model, build: &dyn Fn(&mut MeshBuilder)| {
        let mut builder = MeshBuilder::default();
        build(&mut builder);
        let (mut min, mut max) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
        for vertex in &builder.vertices {
            let pos = Vec3::from_array(vertex.pos);
            min = min.min(pos);
            max = max.max(pos);
        }
        model.meshes.push(MeshRange {
            first_index: model.indices.len() as u32,
            index_count: builder.indices.len() as u32,
            base_vertex: model.vertices.len() as i32,
            first_edge: model.edge_vertices.len() as u32,
            edge_count: builder.edges.len() as u32,
            min,
            max,
        });
        model.vertices.extend_from_slice(&builder.vertices);
        model.indices.extend_from_slice(&builder.indices);
        model.edge_vertices.extend_from_slice(&builder.edges);
        model.meshes.len() - 1
    };

    let gear = |teeth: u32, width: f32, bore: f32, hub: f32, hub_width: f32| {
        move |mesh: &mut MeshBuilder| {
            let (outline, sharp) = gear_outline(teeth, MODULE);
            mesh.extrude_ring(&outline, &sharp, hub, -width * 0.5, width * 0.5);
            let half = hub_width * 0.5;
            mesh.revolve(
                &[
                    (bore, -half),
                    (hub, -half),
                    (hub, half),
                    (bore, half),
                    (bore, -half),
                ],
                SEGMENTS,
            );
        }
    };
    let shaft = |z0: f32, z1: f32| {
        move |mesh: &mut MeshBuilder| {
            mesh.revolve(
                &[
                    (0.0, z0),
                    (11.0, z0),
                    (13.0, z0 + 2.0),
                    (13.0, -74.0),
                    (15.0, -74.0),
                    (15.0, -52.0),
                    (18.0, -52.0),
                    (18.0, 52.0),
                    (15.0, 52.0),
                    (15.0, 92.0),
                    (13.0, 92.0),
                    (13.0, z1 - 2.0),
                    (11.0, z1),
                    (0.0, z1),
                ],
                SEGMENTS,
            );
        }
    };

    let gear_16 = add_mesh(&mut model, &gear(16, 30.0, 18.0, 23.0, 38.0));
    let gear_48 = add_mesh(&mut model, &gear(48, 30.0, 18.0, 34.0, 42.0));
    let gear_18 = add_mesh(&mut model, &gear(18, 30.0, 18.0, 24.0, 38.0));
    let gear_54 = add_mesh(&mut model, &gear(54, 30.0, 18.0, 36.0, 42.0));
    let shaft_input = add_mesh(&mut model, &shaft(-170.0, 112.0));
    let shaft_middle = add_mesh(&mut model, &shaft(-98.0, 112.0));
    let shaft_output = add_mesh(&mut model, &shaft(-98.0, 170.0));
    let bearing = add_mesh(&mut model, &|mesh: &mut MeshBuilder| {
        mesh.revolve(
            &[
                (15.0, -9.0),
                (24.0, -9.0),
                (24.0, -7.0),
                (26.0, -7.0),
                (26.0, -9.0),
                (35.0, -9.0),
                (37.0, -7.0),
                (37.0, 7.0),
                (35.0, 9.0),
                (26.0, 9.0),
                (26.0, 7.0),
                (24.0, 7.0),
                (24.0, 9.0),
                (15.0, 9.0),
                (15.0, -9.0),
            ],
            SEGMENTS,
        );
    });
    let (base_min, base_max) = (Vec3::new(-205.0, -105.0, -134.0), Vec3::new(285.0, 112.0, -115.0));
    let (wall_min, wall_max) = (Vec3::new(-205.0, 78.0, -115.0), Vec3::new(285.0, 92.0, 178.0));
    let base = add_mesh(&mut model, &|mesh: &mut MeshBuilder| mesh.cuboid(base_min, base_max));
    let wall = add_mesh(&mut model, &|mesh: &mut MeshBuilder| mesh.cuboid(wall_min, wall_max));
    let pedestal = |height: f32| {
        move |mesh: &mut MeshBuilder| {
            mesh.cuboid(
                Vec3::new(-42.0, -7.0, -height),
                Vec3::new(42.0, 7.0, 42.0),
            );
        }
    };
    let pedestal_low = add_mesh(&mut model, &pedestal(115.0));
    let pedestal_high = add_mesh(&mut model, &pedestal(164.3));
    let bolt = add_mesh(&mut model, &|mesh: &mut MeshBuilder| {
        let polygon: Vec<Vec3> = (0..6)
            .map(|side| {
                let angle = TAU * side as f32 / 6.0;
                Vec3::new(8.5 * angle.cos(), 8.5 * angle.sin(), 0.0)
            })
            .collect();
        mesh.prism(&polygon, 2.0, 9.0);
        mesh.revolve(
            &[(5.0, 0.0), (11.0, 0.0), (11.0, 2.0), (5.0, 2.0), (5.0, 0.0)],
            48,
        );
    });

    // Local Z becomes world +Y for everything on a shaft.
    let on_shaft = |x: f32, y: f32, z: f32| {
        Mat4::from_translation(Vec3::new(x, y, z)) * Mat4::from_rotation_x(-FRAC_PI_2)
    };
    let mut part = |name: String, mesh: usize, color: u32, base: Mat4, spin: f32, phase: f32| {
        model.parts.push(Part {
            name,
            mesh,
            color: srgb(color),
            base,
            spin,
            phase,
        });
    };

    // Stage 1 16:48, stage 2 18:54; both 1:3, 1:9 overall.
    let distance_1 = MODULE * (16 + 48) as f32 * 0.5;
    let distance_2 = MODULE * (18 + 54) as f32 * 0.5;
    let beta = 20f32.to_radians();
    let a = (-distance_1, 0.0);
    let b = (0.0, 0.0);
    let c = (distance_2 * beta.cos(), distance_2 * beta.sin());

    let spin_a = 0.9;
    let spin_b = -spin_a * 16.0 / 48.0;
    let spin_c = -spin_b * 18.0 / 54.0;
    // Local Y is world -Z, so the angle towards world (x, z) is atan2(-z, x).
    let phase_a = 0.0;
    let phase_b = mesh_phase(phase_a, 16, 48, 0.0);
    let phase_b2 = 0.2;
    let phase_c = mesh_phase(phase_b2, 18, 54, (-c.1).atan2(c.0));

    let (bronze, steel, blue, silver, slate, wall_color, oxide) =
        (0xc68a4a, 0xbcc3cc, 0x5a8fc0, 0x8f9aa8, 0x4b5563, 0x667182, 0x2b2f37);

    part("输入轴 A".into(), shaft_input, steel, on_shaft(a.0, 0.0, a.1), spin_a, 0.0);
    part("中间轴 B".into(), shaft_middle, steel, on_shaft(b.0, 0.0, b.1), spin_b, 0.0);
    part("输出轴 C".into(), shaft_output, steel, on_shaft(c.0, 0.0, c.1), spin_c, 0.0);
    part("小齿轮 Z16".into(), gear_16, bronze, on_shaft(a.0, -30.0, a.1), spin_a, phase_a);
    part("大齿轮 Z48".into(), gear_48, blue, on_shaft(b.0, -30.0, b.1), spin_b, phase_b);
    part("小齿轮 Z18".into(), gear_18, bronze, on_shaft(b.0, 30.0, b.1), spin_b, phase_b2);
    part("大齿轮 Z54".into(), gear_54, blue, on_shaft(c.0, 30.0, c.1), spin_c, phase_c);

    for (name, center, spin) in [("A", a, spin_a), ("B", b, spin_b), ("C", c, spin_c)] {
        part(
            format!("前轴承 {name}"),
            bearing,
            silver,
            on_shaft(center.0, -71.0, center.1),
            spin * 0.5,
            0.0,
        );
        part(
            format!("后轴承 {name}"),
            bearing,
            silver,
            on_shaft(center.0, 85.0, center.1),
            spin * 0.5,
            0.0,
        );
    }

    part("底座".into(), base, slate, Mat4::IDENTITY, 0.0, 0.0);
    part("后壁板".into(), wall, wall_color, Mat4::IDENTITY, 0.0, 0.0);
    let pedestal_at = |center: (f32, f32)| Mat4::from_translation(Vec3::new(center.0, -71.0, center.1));
    part("前支座 A".into(), pedestal_low, wall_color, pedestal_at(a), 0.0, 0.0);
    part("前支座 B".into(), pedestal_low, wall_color, pedestal_at(b), 0.0, 0.0);
    part("前支座 C".into(), pedestal_high, wall_color, pedestal_at(c), 0.0, 0.0);

    let mut bolt_number = 0;
    let up = |x: f32, y: f32| Mat4::from_translation(Vec3::new(x, y, -115.0));
    for (x, y) in [
        (-190.0, -92.0),
        (-190.0, 40.0),
        (230.0, -92.0),
        (270.0, -92.0),
        (270.0, 40.0),
        (-180.0, -71.0),
        (-76.0, -71.0),
        (-52.0, -71.0),
        (52.0, -71.0),
        (83.0, -71.0),
        (187.0, -71.0),
    ] {
        bolt_number += 1;
        part(format!("螺栓 M10-{bolt_number:02}"), bolt, oxide, up(x, y), 0.0, 0.0);
    }
    let facing_front = |x: f32, z: f32| {
        Mat4::from_translation(Vec3::new(x, 78.0, z)) * Mat4::from_rotation_x(FRAC_PI_2)
    };
    for (x, z) in [
        (-190.0, 160.0),
        (-60.0, 160.0),
        (60.0, 160.0),
        (270.0, 160.0),
        (-190.0, -95.0),
        (270.0, -95.0),
    ] {
        bolt_number += 1;
        part(format!("螺栓 M10-{bolt_number:02}"), bolt, oxide, facing_front(x, z), 0.0, 0.0);
    }

    for part in &model.parts {
        let mesh = &model.meshes[part.mesh];
        for corner in 0..8 {
            let local = Vec3::new(
                if corner & 1 == 0 { mesh.min.x } else { mesh.max.x },
                if corner & 2 == 0 { mesh.min.y } else { mesh.max.y },
                if corner & 4 == 0 { mesh.min.z } else { mesh.max.z },
            );
            let world = part.base.transform_point3(local);
            model.min = model.min.min(world);
            model.max = model.max.max(world);
        }
    }
    model
}
