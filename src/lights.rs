#![allow(clippy::needless_range_loop)]

use std::io::Cursor;

use image::codecs::gif::GifDecoder;
use image::{AnimationDecoder, RgbaImage};

use crate::protocol::{LED_COLOR_SCALE, NUM_PANELS};

/// Animation slot type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LightsType {
    Released = 0,
    Pressed = 1,
}

// ─── GIF Decoding ────────────────────────────────────────────────────────────

struct DecodedFrame {
    rgba: RgbaImage,
    duration_secs: f32,
}

fn decode_gif(data: &[u8]) -> Result<Vec<DecodedFrame>, &'static str> {
    let decoder = GifDecoder::new(Cursor::new(data))
        .map_err(|_| "The GIF couldn't be read.")?;

    let frames: Vec<DecodedFrame> = decoder
        .into_frames()
        .filter_map(|f| f.ok())
        .map(|frame| {
            let (numer, denom) = frame.delay().numer_denom_ms();
            let ms = numer as f32 / denom as f32;
            let duration_secs = if ms <= 0.0 || (28.0..=42.0).contains(&ms) {
                1.0 / 30.0
            } else {
                ms / 1000.0
            };
            DecodedFrame {
                rgba: frame.into_buffer(),
                duration_secs,
            }
        })
        .collect();

    if frames.is_empty() {
        return Err("The GIF has no frames.");
    }
    Ok(frames)
}

// ─── Panel Extraction ────────────────────────────────────────────────────────

/// RGB data for a single panel's LEDs in one frame.
#[derive(Clone)]
struct PanelFrame {
    rgb: Vec<u8>, // num_leds * 3
    num_leds: usize,
}

/// Extract a panel's LED data from a decoded RGBA frame.
fn extract_panel(frame: &RgbaImage, panel: usize) -> PanelFrame {
    let col = panel % 3;
    let row = panel / 3;
    let w = frame.width() as usize;

    if w == 14 {
        // 14×15: each panel is 4×4 at (col*5, row*5).
        let mut rgb = vec![0u8; 16 * 3];
        let base_x = col * 5;
        let base_y = row * 5;
        let mut led = 0;
        for dy in 0..4 {
            for dx in 0..4 {
                let px = frame.get_pixel((base_x + dx) as u32, (base_y + dy) as u32);
                rgb[led * 3] = px[0];
                rgb[led * 3 + 1] = px[1];
                rgb[led * 3 + 2] = px[2];
                led += 1;
            }
        }
        PanelFrame { rgb, num_leds: 16 }
    } else {
        // 23×24: outer 4×4 at even coords, inner 3×3 at odd coords.
        let mut rgb = vec![0u8; 25 * 3];
        let base_x = col * 8;
        let base_y = row * 8;
        let mut led = 0;
        // Outer 4×4.
        for dy in 0..4 {
            for dx in 0..4 {
                let px = frame.get_pixel((base_x + dx * 2) as u32, (base_y + dy * 2) as u32);
                rgb[led * 3] = px[0];
                rgb[led * 3 + 1] = px[1];
                rgb[led * 3 + 2] = px[2];
                led += 1;
            }
        }
        // Inner 3×3.
        for dy in 0..3 {
            for dx in 0..3 {
                let px = frame.get_pixel(
                    (base_x + dx * 2 + 1) as u32,
                    (base_y + dy * 2 + 1) as u32,
                );
                rgb[led * 3] = px[0];
                rgb[led * 3 + 1] = px[1];
                rgb[led * 3 + 2] = px[2];
                led += 1;
            }
        }
        PanelFrame { rgb, num_leds: 25 }
    }
}

// ─── Animation Data ──────────────────────────────────────────────────────────

/// Per-panel animation data loaded from a GIF.
#[derive(Clone)]
pub struct PanelAnimationData {
    frames: [Vec<PanelFrame>; NUM_PANELS],
    durations: Vec<f32>,
    loop_frame: usize,
}

/// Playback state for a single panel's animation.
#[derive(Clone, Default)]
struct PlaybackState {
    current_frame: usize,
    time_in_frame: f32,
    playing: bool,
}

impl PlaybackState {
    fn reset(&mut self) {
        self.current_frame = 0;
        self.time_in_frame = 0.0;
    }
}

/// Full animation state for both pads.
pub struct AnimationState {
    animations: [[Option<PanelAnimationData>; 2]; 2], // [pad][type]
    playback: [[[PlaybackState; NUM_PANELS]; 2]; 2],  // [pad][type][panel]
}

impl Default for AnimationState {
    fn default() -> Self {
        Self::new()
    }
}

impl AnimationState {
    pub fn new() -> Self {
        Self {
            animations: Default::default(),
            playback: std::array::from_fn(|_| std::array::from_fn(|_| std::array::from_fn(|_| PlaybackState::default()))),
        }
    }

    /// Load a GIF animation into a slot.
    pub fn load(
        &mut self,
        gif_data: &[u8],
        pad: usize,
        lights_type: LightsType,
    ) -> Result<(), &'static str> {
        if pad > 1 {
            return Err("Invalid pad index.");
        }

        let frames = decode_gif(gif_data)?;
        let w = frames[0].rgba.width();
        let h = frames[0].rgba.height();

        if !((w == 14 && h == 15) || (w == 23 && h == 24)) {
            return Err("The GIF must be 14x15 or 23x24.");
        }

        let mut loop_frame = 0;
        let mut panel_frames: [Vec<PanelFrame>; NUM_PANELS] = std::array::from_fn(|_| Vec::new());
        let mut durations = Vec::with_capacity(frames.len());

        for (f_idx, frame) in frames.iter().enumerate() {
            // Check loop marker: bottom-left pixel is white-ish.
            let marker_y = h - 1;
            let marker_px = frame.rgba.get_pixel(0, marker_y);
            if marker_px[3] == 255 && marker_px[0] >= 128 && loop_frame == 0 {
                loop_frame = f_idx;
            }

            durations.push(frame.duration_secs);
            for panel in 0..NUM_PANELS {
                panel_frames[panel].push(extract_panel(&frame.rgba, panel));
            }
        }

        let anim = PanelAnimationData {
            frames: panel_frames,
            durations,
            loop_frame,
        };

        let type_idx = lights_type as usize;
        self.animations[pad][type_idx] = Some(anim);
        // Reset playback for this slot.
        for panel in 0..NUM_PANELS {
            self.playback[pad][type_idx][panel] = PlaybackState::default();
        }

        Ok(())
    }

    /// Build a frame of light data for both pads (returns BYTES_PER_PAD_25 * 2 bytes).
    /// `input_states` is [pad0_state, pad1_state] bitmasks.
    pub fn build_frame(&mut self, input_states: [u16; 2]) -> Vec<u8> {
        let dt = 1.0 / 30.0f32;
        let bytes_per_panel = 25 * 3;
        let bytes_per_pad = NUM_PANELS * bytes_per_panel;
        let mut light_data = vec![0u8; bytes_per_pad * 2];

        for pad in 0..2 {
            for panel in 0..NUM_PANELS {
                let out_offset = pad * bytes_per_pad + panel * bytes_per_panel;
                let out = &mut light_data[out_offset..out_offset + bytes_per_panel];

                // Released animation (always playing).
                if let Some(ref anim) = self.animations[pad][LightsType::Released as usize] {
                    let ps = &mut self.playback[pad][LightsType::Released as usize][panel];
                    ps.playing = true;
                    let pf = &anim.frames[panel][ps.current_frame];
                    let copy_len = bytes_per_panel.min(pf.num_leds * 3);
                    out[..copy_len].copy_from_slice(&pf.rgb[..copy_len]);
                }

                // Pressed animation overlay.
                let pressed = (input_states[pad] & (1 << panel)) != 0;
                if let Some(ref anim) = self.animations[pad][LightsType::Pressed as usize] {
                    let ps = &mut self.playback[pad][LightsType::Pressed as usize][panel];
                    if pressed {
                        ps.playing = true;
                        let pf = &anim.frames[panel][ps.current_frame];
                        // Overlay non-black pixels.
                        for i in 0..pf.num_leds.min(25) {
                            let r = pf.rgb[i * 3];
                            let g = pf.rgb[i * 3 + 1];
                            let b = pf.rgb[i * 3 + 2];
                            if r != 0 || g != 0 || b != 0 {
                                out[i * 3] = r;
                                out[i * 3 + 1] = g;
                                out[i * 3 + 2] = b;
                            }
                        }
                    } else {
                        ps.playing = false;
                        ps.reset();
                    }
                }
            }

            // Advance timing.
            for type_idx in 0..2 {
                let Some(ref anim) = self.animations[pad][type_idx] else { continue };
                for panel in 0..NUM_PANELS {
                    let ps = &mut self.playback[pad][type_idx][panel];
                    if !ps.playing {
                        continue;
                    }
                    ps.time_in_frame += dt;
                    if ps.time_in_frame >= anim.durations[ps.current_frame] {
                        ps.time_in_frame -= anim.durations[ps.current_frame];
                        ps.current_frame += 1;
                        if ps.current_frame >= anim.durations.len() {
                            ps.current_frame = anim.loop_frame;
                        }
                    }
                }
            }
        }

        light_data
    }
}

// ─── Firmware Upload Preparation ─────────────────────────────────────────────

/// Prepared upload data ready to be sent to a device.
pub struct UploadData {
    pub commands: Vec<UploadCommand>,
}

/// A single command in the upload sequence.
pub enum UploadCommand {
    /// Data packet to send to the device.
    Packet(Vec<u8>),
    /// Delay in milliseconds before next packet.
    Delay(u16),
}

/// Prepares animation data for firmware upload (EEPROM write).
/// Only 23×24 GIFs (25-LED mode) are supported for upload.
/// Max 32 frames per animation type, max 15 colors per panel.
pub fn prepare_upload(
    gif_data: &[u8],
    pad: usize,
    lights_type: LightsType,
) -> Result<UploadData, &'static str> {
    if pad > 1 {
        return Err("Invalid pad index.");
    }

    let frames = decode_gif(gif_data)?;
    let w = frames[0].rgba.width();
    let h = frames[0].rgba.height();

    if w != 23 || h != 24 {
        return Err("Upload GIFs must be 23x24.");
    }
    if frames.len() > 32 {
        return Err("The animation has too many frames (max 32).");
    }

    // Extract per-panel frames and detect loop frame.
    let mut loop_frame: u8 = 0;
    let mut panel_frames: [Vec<PanelFrame>; NUM_PANELS] = std::array::from_fn(|_| Vec::new());
    let mut durations = Vec::new();

    for (f_idx, frame) in frames.iter().enumerate() {
        let marker_px = frame.rgba.get_pixel(0, h - 1);
        if marker_px[3] == 255 && marker_px[0] >= 128 && loop_frame == 0 {
            loop_frame = f_idx as u8;
        }
        durations.push(frame.duration_secs);
        for panel in 0..NUM_PANELS {
            panel_frames[panel].push(extract_panel(&frame.rgba, panel));
        }
    }

    let first_graphic: usize = if lights_type == LightsType::Released { 0 } else { 32 };
    let type_idx = lights_type as u8;

    // Build palette and pack graphics for each panel.
    let mut all_panel_data: Vec<PanelUploadData> = Vec::with_capacity(NUM_PANELS);
    for panel in 0..NUM_PANELS {
        let palette = build_palette(&panel_frames[panel])?;
        let mut graphics = vec![[0u8; 13]; 32]; // 32 graphics slots
        for (f, pf) in panel_frames[panel].iter().enumerate() {
            graphics[f] = pack_graphic(pf, &palette);
        }
        all_panel_data.push(PanelUploadData { palette, graphics });
    }

    // Build timing data.
    let mut timing_frames = [0xFFu8; 64];
    let mut timing_delays = [0u8; 64];
    for (f, dur) in durations.iter().enumerate() {
        timing_frames[f] = (first_graphic + f) as u8;
        timing_delays[f] = (dur * 30.0 + 0.5).clamp(1.0, 255.0) as u8;
    }

    // Generate upload command sequence.
    let mut commands: Vec<UploadCommand> = Vec::new();

    // Per-panel upload packets (interleaved across panels with delays).
    let mut panel_packets: [Vec<Vec<u8>>; NUM_PANELS] = std::array::from_fn(|_| Vec::new());
    for panel in 0..NUM_PANELS {
        let pd = &all_panel_data[panel];

        // Graphics data.
        let graphics_offset = first_graphic * 13;
        let graphics_bytes: Vec<u8> = pd.graphics.iter().flat_map(|g| g.iter().copied()).collect();
        create_upload_packets(
            &mut panel_packets[panel],
            &graphics_bytes,
            graphics_offset as u16,
            panel as u8,
            type_idx,
        );

        // Palette data (scale colors).
        let palette_offset = 64 * 13 + (type_idx as usize) * 45;
        let mut palette_bytes = [0u8; 45];
        for (i, color) in pd.palette.iter().enumerate() {
            palette_bytes[i * 3] = (color[0] as f32 * LED_COLOR_SCALE) as u8;
            palette_bytes[i * 3 + 1] = (color[1] as f32 * LED_COLOR_SCALE) as u8;
            palette_bytes[i * 3 + 2] = (color[2] as f32 * LED_COLOR_SCALE) as u8;
        }
        create_upload_packets(
            &mut panel_packets[panel],
            &palette_bytes,
            palette_offset as u16,
            panel as u8,
            type_idx,
        );
    }

    // Interleave packets across panels with EEPROM write delays.
    loop {
        let mut added_any = false;
        let mut max_size: u16 = 0;
        for panel in 0..NUM_PANELS {
            if let Some(pkt) = panel_packets[panel].pop() {
                let size = pkt[6] as u16; // size field in packet
                max_size = max_size.max(size);
                commands.push(UploadCommand::Packet(pkt));
                added_any = true;
            }
        }
        if !added_any {
            break;
        }
        let delay = (max_size as f32 * 3.4 + 0.5) as u16;
        commands.push(UploadCommand::Delay(delay));
    }

    // Send panel data twice for reliability.
    let panel_data_len = commands.len();
    let panel_commands: Vec<UploadCommand> = commands
        .iter()
        .map(|c| match c {
            UploadCommand::Packet(p) => UploadCommand::Packet(p.clone()),
            UploadCommand::Delay(d) => UploadCommand::Delay(*d),
        })
        .collect();
    commands.extend(panel_commands.into_iter().take(panel_data_len));

    // Master timing data.
    let mut timing_data = Vec::with_capacity(129);
    timing_data.push(loop_frame);
    timing_data.extend_from_slice(&timing_frames);
    timing_data.extend_from_slice(&timing_delays);

    let mut master_packets = Vec::new();
    create_upload_packets(&mut master_packets, &timing_data, 0, 0xFF, type_idx);
    // Mark last packet as final.
    if let Some(last) = master_packets.last_mut() {
        last[3] = 1; // final_packet field
    }
    for pkt in master_packets {
        commands.push(UploadCommand::Packet(pkt));
    }

    Ok(UploadData { commands })
}

// ─── Upload Helpers ──────────────────────────────────────────────────────────

struct PanelUploadData {
    palette: [[u8; 3]; 15],
    graphics: Vec<[u8; 13]>,
}

fn build_palette(frames: &[PanelFrame]) -> Result<[[u8; 3]; 15], &'static str> {
    let mut palette = [[0u8; 3]; 15];
    let mut count = 0;

    for frame in frames {
        for i in 0..frame.num_leds {
            let r = frame.rgb[i * 3];
            let g = frame.rgb[i * 3 + 1];
            let b = frame.rgb[i * 3 + 2];
            if r == 0 && g == 0 && b == 0 {
                continue; // transparent
            }
            // Check if already in palette.
            let found = palette[..count].iter().any(|c| c[0] == r && c[1] == g && c[2] == b);
            if found {
                continue;
            }
            if count >= 15 {
                return Err("A panel uses too many colors (max 15).");
            }
            palette[count] = [r, g, b];
            count += 1;
        }
    }
    Ok(palette)
}

fn find_in_palette(palette: &[[u8; 3]; 15], r: u8, g: u8, b: u8) -> u8 {
    for (i, c) in palette.iter().enumerate() {
        if c[0] == r && c[1] == g && c[2] == b {
            return i as u8;
        }
    }
    0xFF
}

/// Pack 25 pixels into 13 bytes (4-bit nibbles, high nibble first).
fn pack_graphic(frame: &PanelFrame, palette: &[[u8; 3]; 15]) -> [u8; 13] {
    let mut out = [0u8; 13];
    for i in 0..25 {
        let pal_idx = if i >= frame.num_leds {
            15 // transparent
        } else {
            let r = frame.rgb[i * 3];
            let g = frame.rgb[i * 3 + 1];
            let b = frame.rgb[i * 3 + 2];
            if r == 0 && g == 0 && b == 0 {
                15
            } else {
                let idx = find_in_palette(palette, r, g, b);
                if idx == 0xFF { 0 } else { idx }
            }
        };
        if i & 1 == 0 {
            out[i / 2] |= (pal_idx & 0x0F) << 4;
        } else {
            out[i / 2] |= pal_idx & 0x0F;
        }
    }
    out
}

/// Create upload packets for a block of data.
/// Packet format: [cmd='m'][panel][animation_idx][final_packet][offset_lo][offset_hi][size][data...]
fn create_upload_packets(
    packets: &mut Vec<Vec<u8>>,
    data: &[u8],
    start_offset: u16,
    panel: u8,
    anim_idx: u8,
) {
    const MAX_DATA_PER_PACKET: usize = 240;
    let mut offset = 0;
    while offset < data.len() {
        let chunk_size = MAX_DATA_PER_PACKET.min(data.len() - offset);
        let pkt_offset = start_offset + offset as u16;

        let mut pkt = Vec::with_capacity(7 + chunk_size);
        pkt.push(b'm');
        pkt.push(panel);
        pkt.push(anim_idx);
        pkt.push(0); // final_packet (set later for last master packet)
        pkt.extend_from_slice(&pkt_offset.to_le_bytes());
        pkt.push(chunk_size as u8);
        pkt.extend_from_slice(&data[offset..offset + chunk_size]);

        packets.push(pkt);
        offset += chunk_size;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_extraction_14x15() {
        // Create a minimal 14×15 RGBA image with known pixel values.
        let mut img = RgbaImage::new(14, 15);
        // Set panel 0 (top-left, at 0,0) first pixel to red.
        img.put_pixel(0, 0, image::Rgba([255, 0, 0, 255]));
        // Set panel 4 (center, at col=1 row=1, base_x=5 base_y=5) first pixel to green.
        img.put_pixel(5, 5, image::Rgba([0, 255, 0, 255]));

        let pf0 = extract_panel(&img, 0);
        assert_eq!(pf0.num_leds, 16);
        assert_eq!(pf0.rgb[0], 255); // R
        assert_eq!(pf0.rgb[1], 0);   // G
        assert_eq!(pf0.rgb[2], 0);   // B

        let pf4 = extract_panel(&img, 4);
        assert_eq!(pf4.rgb[0], 0);
        assert_eq!(pf4.rgb[1], 255);
        assert_eq!(pf4.rgb[2], 0);
    }

    #[test]
    fn pack_graphic_roundtrip() {
        let palette: [[u8; 3]; 15] = {
            let mut p = [[0u8; 3]; 15];
            p[0] = [255, 0, 0];
            p[1] = [0, 255, 0];
            p
        };

        let mut rgb = vec![0u8; 25 * 3];
        // LED 0 = red (palette index 0)
        rgb[0] = 255;
        // LED 1 = green (palette index 1)
        rgb[4] = 255;

        let frame = PanelFrame { rgb, num_leds: 25 };
        let packed = pack_graphic(&frame, &palette);

        // LED 0 → index 0 in high nibble of byte 0.
        // LED 1 → index 1 in low nibble of byte 0.
        assert_eq!(packed[0], 0x01);
        // LED 2 onwards are black → index 15.
        assert_eq!(packed[1], 0xFF);
    }
}
