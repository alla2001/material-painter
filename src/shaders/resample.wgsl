// Bilinear copy between texture sizes, used when the texture set resolution changes.

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;

@fragment
fn fs_resample(in: FsOut) -> @location(0) vec4<f32> {
    return textureSampleLevel(src, samp, in.uv, 0.0);
}

// A gray image as a painted mask: its value as the smart mask's multiplier,
// nothing added on top.
@fragment
fn fs_resample_mask(in: FsOut) -> @location(0) vec4<f32> {
    return vec4<f32>(textureSampleLevel(src, samp, in.uv, 0.0).r, 0.0, 0.0, 1.0);
}
