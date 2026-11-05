struct Globals {
    view_proj: mat4x4<f32>,
    eye: vec4<f32>,
    // xyz: the camera's forward direction; w: 1 when orthographic.
    forward: vec4<f32>,
    // xyz: the section plane's normal; w: its distance from the origin.
    section: vec4<f32>,
    // x: the radius the grid fades out at, 0 for no fade.
    grid: vec4<f32>,
    // hovered part, selected part, display mode, section enabled.
    ids: vec4<u32>,
};

@group(0) @binding(0) var<uniform> g: Globals;

const MODE_SHADED_EDGES: u32 = 1u;
const MODE_HIDDEN_LINE: u32 = 2u;
const HOVER_TINT: vec3<f32> = vec3<f32>(1.0, 0.55, 0.08);
const SELECT_TINT: vec3<f32> = vec3<f32>(0.10, 0.50, 1.0);
const BLUEPRINT_FILL: vec3<f32> = vec3<f32>(0.016, 0.026, 0.045);
const BLUEPRINT_LINE: vec3<f32> = vec3<f32>(0.70, 0.80, 0.92);

struct InstanceInput {
    @location(2) m0: vec4<f32>,
    @location(3) m1: vec4<f32>,
    @location(4) m2: vec4<f32>,
    @location(5) m3: vec4<f32>,
    @location(6) color: vec3<f32>,
    @location(7) id: u32,
};

fn cut_away(world: vec3<f32>) -> bool {
    return g.ids.w != 0u && dot(world, g.section.xyz) > g.section.w;
}

// ---------------------------------------------------------------- background

struct BackgroundOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) v: f32,
};

@vertex
fn vs_background(@builtin(vertex_index) index: u32) -> BackgroundOut {
    let uv = vec2<f32>(f32((index << 1u) & 2u), f32(index & 2u));
    var out: BackgroundOut;
    out.clip = vec4<f32>(uv * 2.0 - 1.0, 0.0, 1.0);
    out.v = uv.y;
    return out;
}

@fragment
fn fs_background(in: BackgroundOut) -> @location(0) vec4<f32> {
    let bottom = vec3<f32>(0.020, 0.024, 0.032);
    let top = vec3<f32>(0.060, 0.072, 0.095);
    return vec4<f32>(mix(bottom, top, in.v), 1.0);
}

// -------------------------------------------------------------------- solids

struct SolidOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) world: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) color: vec3<f32>,
    @location(3) @interpolate(flat) id: u32,
};

@vertex
fn vs_solid(
    @location(0) pos: vec3<f32>,
    @location(1) normal: vec3<f32>,
    instance: InstanceInput,
) -> SolidOut {
    let model = mat4x4<f32>(instance.m0, instance.m1, instance.m2, instance.m3);
    let world = model * vec4<f32>(pos, 1.0);
    var out: SolidOut;
    out.clip = g.view_proj * world;
    out.world = world.xyz;
    out.normal = (model * vec4<f32>(normal, 0.0)).xyz;
    out.color = instance.color;
    out.id = instance.id;
    return out;
}

@fragment
fn fs_solid(in: SolidOut, @builtin(front_facing) front: bool) -> @location(0) vec4<f32> {
    if (cut_away(in.world)) {
        discard;
    }
    // Seen through the section plane, a back face is the inside of a cut
    // solid: hatch it, as a section view does.
    if (!front && g.ids.w != 0u) {
        let stripe = step(0.5, fract((in.clip.x + in.clip.y) / 10.0));
        return vec4<f32>(mix(vec3<f32>(0.50, 0.075, 0.05), vec3<f32>(0.80, 0.16, 0.09), stripe), 1.0);
    }
    if (g.ids.z == MODE_HIDDEN_LINE) {
        return vec4<f32>(BLUEPRINT_FILL, 1.0);
    }

    var n = normalize(in.normal);
    if (!front) {
        n = -n;
    }
    var to_eye = normalize(g.eye.xyz - in.world);
    if (g.forward.w > 0.5) {
        to_eye = -g.forward.xyz;
    }

    let sky = vec3<f32>(0.42, 0.46, 0.54);
    let ground = vec3<f32>(0.11, 0.10, 0.09);
    let ambient = mix(ground, sky, n.z * 0.5 + 0.5);
    let headlight = max(dot(n, to_eye), 0.0);
    let fill = max(dot(n, normalize(vec3<f32>(-0.45, -0.65, 0.62))), 0.0);
    let specular = pow(headlight, 42.0);

    var color = in.color * (ambient * 0.62 + vec3<f32>(0.62 * headlight + 0.26 * fill));
    color += vec3<f32>(0.16 * specular);

    if (in.id == g.ids.y) {
        color = mix(color, SELECT_TINT, 0.55);
    } else if (in.id == g.ids.x) {
        color = mix(color, HOVER_TINT, 0.40);
    }
    return vec4<f32>(color, 1.0);
}

// ------------------------------------------------------------- feature edges

struct EdgeOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) world: vec3<f32>,
    @location(1) @interpolate(flat) id: u32,
};

@vertex
fn vs_edge(@location(0) pos: vec3<f32>, instance: InstanceInput) -> EdgeOut {
    let model = mat4x4<f32>(instance.m0, instance.m1, instance.m2, instance.m3);
    let world = model * vec4<f32>(pos, 1.0);
    var out: EdgeOut;
    out.clip = g.view_proj * world;
    out.world = world.xyz;
    out.id = instance.id;
    return out;
}

@fragment
fn fs_edge(in: EdgeOut) -> @location(0) vec4<f32> {
    if (cut_away(in.world)) {
        discard;
    }
    var color = vec3<f32>(0.006, 0.007, 0.010);
    if (g.ids.z == MODE_HIDDEN_LINE) {
        color = BLUEPRINT_LINE;
    }
    if (in.id == g.ids.y) {
        color = SELECT_TINT;
    } else if (in.id == g.ids.x) {
        color = HOVER_TINT;
    }
    return vec4<f32>(color, 1.0);
}

// ------------------------------------------------------- grid and axis triad

struct LineOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) world: vec3<f32>,
    @location(1) color: vec4<f32>,
};

@vertex
fn vs_line(@location(0) pos: vec3<f32>, @location(1) color: vec4<f32>) -> LineOut {
    var out: LineOut;
    out.clip = g.view_proj * vec4<f32>(pos, 1.0);
    out.world = pos;
    out.color = color;
    return out;
}

@fragment
fn fs_line(in: LineOut) -> @location(0) vec4<f32> {
    var alpha = in.color.a;
    if (g.grid.x > 0.0) {
        alpha *= 1.0 - smoothstep(g.grid.x * 0.45, g.grid.x, length(in.world.xy - g.grid.yz));
    }
    return vec4<f32>(in.color.rgb, alpha);
}

// -------------------------------------------------------------------- picking

struct PickOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) world: vec3<f32>,
    @location(1) @interpolate(flat) id: u32,
};

@vertex
fn vs_pick(@location(0) pos: vec3<f32>, instance: InstanceInput) -> PickOut {
    let model = mat4x4<f32>(instance.m0, instance.m1, instance.m2, instance.m3);
    let world = model * vec4<f32>(pos, 1.0);
    var out: PickOut;
    out.clip = g.view_proj * world;
    out.world = world.xyz;
    out.id = instance.id;
    return out;
}

@fragment
fn fs_pick(in: PickOut) -> @location(0) u32 {
    if (cut_away(in.world)) {
        discard;
    }
    return in.id;
}
