//! A Z-up turntable camera with reversed-Z projections.

use glam::{Mat4, Vec3};
use crate::time::{Duration, Instant};

const FOV_Y: f32 = 35f32.to_radians();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Projection {
    Perspective,
    Orthographic,
}

#[derive(Clone, Copy, Debug)]
pub struct Camera {
    pub target: Vec3,
    pub yaw: f32,
    pub pitch: f32,
    pub distance: f32,
    pub projection: Projection,
}

impl Camera {
    pub fn eye(&self) -> Vec3 {
        let (sin_yaw, cos_yaw) = self.yaw.sin_cos();
        let (sin_pitch, cos_pitch) = self.pitch.sin_cos();
        self.target + Vec3::new(cos_pitch * cos_yaw, cos_pitch * sin_yaw, sin_pitch) * self.distance
    }

    pub fn forward(&self) -> Vec3 {
        (self.target - self.eye()).normalize()
    }

    pub fn right(&self) -> Vec3 {
        self.forward().cross(Vec3::Z).normalize()
    }

    pub fn up(&self) -> Vec3 {
        self.right().cross(self.forward())
    }

    pub fn view(&self) -> Mat4 {
        Mat4::look_at_rh(self.eye(), self.target, Vec3::Z)
    }

    /// Half the visible height at the target's distance. Both projections
    /// share it, so switching between them keeps the model the same size.
    pub fn half_height(&self) -> f32 {
        self.distance * (FOV_Y * 0.5).tan()
    }

    /// `scene_radius` bounds what the orthographic depth range has to hold.
    pub fn projection_matrix(&self, aspect: f32, scene_radius: f32) -> Mat4 {
        match self.projection {
            Projection::Perspective => {
                Mat4::perspective_infinite_reverse_rh(FOV_Y, aspect, (self.distance * 0.01).max(0.01))
            }
            Projection::Orthographic => {
                let half_height = self.half_height();
                let half_width = half_height * aspect;
                let depth = self.distance + scene_radius * 4.0;
                // Near and far swapped: reversed Z, as the perspective one.
                Mat4::orthographic_rh(-half_width, half_width, -half_height, half_height, depth, -depth)
            }
        }
    }

    pub fn orbit(&mut self, dx: f32, dy: f32) {
        const LIMIT: f32 = 89.9 * std::f32::consts::PI / 180.0;
        self.yaw -= dx * 0.006;
        self.pitch = (self.pitch + dy * 0.006).clamp(-LIMIT, LIMIT);
    }

    pub fn pan(&mut self, dx: f32, dy: f32, viewport_height: f32) {
        let per_pixel = 2.0 * self.half_height() / viewport_height.max(1.0);
        let (right, up) = (self.right(), self.up());
        self.target += (up * dy - right * dx) * per_pixel;
    }

    /// Zooms so the point under the cursor stays under the cursor.
    /// `ndc` is the cursor in normalized device coordinates.
    pub fn zoom_at(&mut self, factor: f32, ndc: (f32, f32), aspect: f32) {
        let half_height = self.half_height();
        let offset = self.right() * ndc.0 * half_height * aspect + self.up() * ndc.1 * half_height;
        self.target += offset * (1.0 - factor);
        self.distance *= factor;
    }
}

/// A short glide between two camera poses, used by the view presets.
pub struct Glide {
    from: Camera,
    to: Camera,
    started: Instant,
    duration: Duration,
}

impl Glide {
    pub fn new(from: Camera, to: Camera) -> Self {
        Self {
            from,
            to,
            started: Instant::now(),
            duration: Duration::from_millis(350),
        }
    }

    /// The pose now, and whether the glide has arrived.
    pub fn sample(&self) -> (Camera, bool) {
        let t = (self.started.elapsed().as_secs_f32() / self.duration.as_secs_f32()).min(1.0);
        let eased = t * t * (3.0 - 2.0 * t);
        let mut yaw_delta = (self.to.yaw - self.from.yaw) % std::f32::consts::TAU;
        if yaw_delta > std::f32::consts::PI {
            yaw_delta -= std::f32::consts::TAU;
        } else if yaw_delta < -std::f32::consts::PI {
            yaw_delta += std::f32::consts::TAU;
        }
        (
            Camera {
                target: self.from.target.lerp(self.to.target, eased),
                yaw: self.from.yaw + yaw_delta * eased,
                pitch: self.from.pitch + (self.to.pitch - self.from.pitch) * eased,
                distance: self.from.distance * (self.to.distance / self.from.distance).powf(eased),
                projection: self.to.projection,
            },
            t >= 1.0,
        )
    }
}
