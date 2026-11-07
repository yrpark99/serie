use std::{
    env,
    io::{self, Write},
    num::NonZeroU16,
};

use base64::Engine;
use ratatui::{
    buffer::CellDiffOption,
    style::{Color, Style},
};

// Kitty-like terminals are detected from env variables, then Sixel support is probed with DA1
// (skipped for terminals that also support the iTerm2 protocol and inside tmux). Otherwise the
// Iterm2 protocol is assumed.
pub fn auto_detect() -> ImageProtocol {
    if detect_kitty_graphics_protocol() {
        if detect_tmux() {
            ImageProtocol::KittyUnicode { tmux: true }
        } else {
            ImageProtocol::Kitty
        }
    } else if !detect_tmux() && !prefers_iterm2() && detect_sixel() {
        let (cell_width_px, cell_height_px) = detect_cell_pixel_size();
        ImageProtocol::Sixel {
            cell_width_px,
            cell_height_px,
        }
    } else {
        ImageProtocol::Iterm2
    }
}

fn detect_kitty_graphics_protocol() -> bool {
    // kitty
    // https://sw.kovidgoyal.net/kitty/glossary/#envvar-KITTY_WINDOW_ID
    env::var("KITTY_WINDOW_ID").is_ok()
    // ghostty
    // https://ghostty.org/docs/help/terminfo
    || env::var("TERM").ok().is_some_and(|t| t == "xterm-ghostty")
    || env::var("GHOSTTY_RESOURCES_DIR").is_ok()
}

pub fn detect_tmux() -> bool {
    env::var("TMUX").is_ok_and(|tmux| !tmux.is_empty())
        || env::var("TERM").is_ok_and(|term| term.starts_with("tmux"))
        || env::var("TERM_PROGRAM").is_ok_and(|term_program| term_program == "tmux")
}

#[derive(Debug, Clone, Copy)]
pub enum ImageProtocol {
    Iterm2,
    Kitty,
    KittyUnicode { tmux: bool },
    /// Sixel has no scaling, so the image is resized to the terminal's cell pixel size.
    Sixel {
        cell_width_px: u16,
        cell_height_px: u16,
    },
}

/// Returns the terminal cell size in pixels, falling back to a common size.
pub fn detect_cell_pixel_size() -> (u16, u16) {
    const FALLBACK: (u16, u16) = (10, 20);
    // Ask the terminal itself first: over SSH the ioctl often reports no pixel size or a bogus one.
    if let Some(size) = query_cell_size_csi().filter(|&s| is_plausible_cell_size(s)) {
        return size;
    }
    if let Ok(w) = ratatui::crossterm::terminal::window_size() {
        if w.columns > 0 && w.rows > 0 {
            let size = (w.width / w.columns, w.height / w.rows);
            if is_plausible_cell_size(size) {
                return size;
            }
        }
    }
    FALLBACK
}

fn is_plausible_cell_size((w, h): (u16, u16)) -> bool {
    (4..=200).contains(&w) && (8..=400).contains(&h)
}

/// Asks the terminal for its cell size with `CSI 16 t` (reply: `CSI 6 ; height ; width t`).
fn query_cell_size_csi() -> Option<(u16, u16)> {
    parse_cell_size_report(&query_terminal(b"\x1b[16t", b't')?)
}

/// Sends `request` and reads the reply up to and including the `end` byte (300ms timeout).
#[cfg(unix)]
fn query_terminal(request: &[u8], end: u8) -> Option<Vec<u8>> {
    use std::{
        io::IsTerminal,
        os::fd::AsRawFd,
        time::{Duration, Instant},
    };

    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return None;
    }
    ratatui::crossterm::terminal::enable_raw_mode().ok()?;
    let result = (|| {
        let mut out = io::stdout().lock();
        out.write_all(request).ok()?;
        out.flush().ok()?;
        let fd = io::stdin().as_raw_fd();
        let deadline = Instant::now() + Duration::from_millis(300);
        let mut buf = Vec::new();
        while Instant::now() < deadline && buf.last() != Some(&end) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: pfd is a valid pollfd and we pass exactly one.
            let n = unsafe { libc::poll(&mut pfd, 1, remaining.as_millis() as i32) };
            if n <= 0 {
                break;
            }
            let mut byte = 0u8;
            // SAFETY: reads at most one byte into a valid buffer.
            let r = unsafe { libc::read(fd, (&mut byte as *mut u8).cast(), 1) };
            if r != 1 {
                break;
            }
            buf.push(byte);
        }
        (buf.last() == Some(&end)).then_some(buf)
    })();
    let _ = ratatui::crossterm::terminal::disable_raw_mode();
    result
}

#[cfg(not(unix))]
fn query_terminal(_request: &[u8], _end: u8) -> Option<Vec<u8>> {
    None
}

/// Detects Sixel support from the DA1 reply (`CSI ? Ps ; ... c`, where attribute 4 is Sixel).
fn detect_sixel() -> bool {
    query_terminal(b"\x1b[c", b'c').is_some_and(|reply| da1_reports_sixel(&reply))
}

fn da1_reports_sixel(reply: &[u8]) -> bool {
    let Ok(s) = std::str::from_utf8(reply) else {
        return false;
    };
    s.rsplit_once("\x1b[?")
        .and_then(|(_, body)| body.strip_suffix('c'))
        .is_some_and(|params| params.split(';').any(|p| p == "4"))
}

// Terminals that also speak the iTerm2 protocol keep using it, as before.
fn prefers_iterm2() -> bool {
    env::var("TERM_PROGRAM")
        .is_ok_and(|p| matches!(p.as_str(), "iTerm.app" | "WezTerm" | "rio" | "vscode"))
}

fn parse_cell_size_report(buf: &[u8]) -> Option<(u16, u16)> {
    let s = std::str::from_utf8(buf).ok()?;
    let body = s.rsplit_once("\x1b[6;")?.1.strip_suffix('t')?;
    let (h, w) = body.split_once(';')?;
    let (w, h) = (w.parse::<u16>().ok()?, h.parse::<u16>().ok()?);
    (w > 0 && h > 0).then_some((w, h))
}

#[derive(Debug, Clone)]
pub struct PreparedImageCell {
    symbol: String,
    style: Style,
    diff_option: CellDiffOption,
}

impl PreparedImageCell {
    pub fn symbol(&self) -> &str {
        &self.symbol
    }

    pub fn style(&self) -> Style {
        self.style
    }

    pub fn diff_option(&self) -> CellDiffOption {
        self.diff_option
    }
}

#[derive(Debug, Clone)]
pub struct PreparedImage {
    cells: Vec<PreparedImageCell>,
    upload_data: Option<String>,
}

impl PreparedImage {
    pub fn cells(&self) -> &[PreparedImageCell] {
        &self.cells
    }

    pub fn take_upload_data(&mut self) -> Option<String> {
        self.upload_data.take()
    }
}

impl ImageProtocol {
    pub fn prepare_image(&self, bytes: &[u8], cell_width: usize, image_id: u32) -> PreparedImage {
        let symbol = match self {
            ImageProtocol::Iterm2 => iterm2_encode(bytes, cell_width, 1),
            ImageProtocol::Kitty => kitty_encode(bytes, cell_width, 1),
            ImageProtocol::KittyUnicode { tmux } => {
                return kitty_unicode_prepare(bytes, cell_width, image_id, *tmux);
            }
            ImageProtocol::Sixel {
                cell_width_px,
                cell_height_px,
            } => sixel_encode(
                bytes,
                cell_width,
                *cell_width_px as u32,
                *cell_height_px as u32,
            ),
        };
        let mut cells = Vec::with_capacity(cell_width);
        cells.push(PreparedImageCell {
            symbol,
            style: Style::default(),
            // The escape sequence occupies one buffer cell. Covered cells are skipped below,
            // so its string width must not make diffing jump past the image or into another row.
            diff_option: CellDiffOption::ForcedWidth(NonZeroU16::MIN),
        });
        for _ in 1..cell_width {
            cells.push(PreparedImageCell {
                symbol: String::new(),
                style: Style::default(),
                diff_option: CellDiffOption::Skip,
            });
        }
        PreparedImage {
            cells,
            upload_data: None,
        }
    }

    pub fn clear_line(&self, y: u16) {
        match self {
            ImageProtocol::Iterm2 | ImageProtocol::Sixel { .. } => {}
            ImageProtocol::Kitty => kitty_clear_line(y),
            ImageProtocol::KittyUnicode { .. } => {}
        }
    }

    /// Sixel pixels are not tracked by the terminal as separate objects, so they can only be
    /// removed by erasing the cells (and forcing a full repaint).
    pub fn requires_full_clear(&self) -> bool {
        matches!(self, ImageProtocol::Sixel { .. })
    }

    pub fn clear(&self) {
        match self {
            ImageProtocol::Iterm2 | ImageProtocol::Sixel { .. } => {}
            ImageProtocol::Kitty => kitty_clear(),
            ImageProtocol::KittyUnicode { .. } => {}
        }
    }

    pub fn delete_images(&self, image_ids: &[u32]) -> Result<(), std::io::Error> {
        match self {
            ImageProtocol::Iterm2 | ImageProtocol::Kitty | ImageProtocol::Sixel { .. } => Ok(()),
            ImageProtocol::KittyUnicode { tmux } => kitty_unicode_delete_images(image_ids, *tmux),
        }
    }
}

const KITTY_PLACEHOLDER: char = '\u{10EEEE}';
static ROW_COLUMN_DIACRITICS: &[char] = &[
    '\u{0305}',
    '\u{030D}',
    '\u{030E}',
    '\u{0310}',
    '\u{0312}',
    '\u{033D}',
    '\u{033E}',
    '\u{033F}',
    '\u{0346}',
    '\u{034A}',
    '\u{034B}',
    '\u{034C}',
    '\u{0350}',
    '\u{0351}',
    '\u{0352}',
    '\u{0357}',
    '\u{035B}',
    '\u{0363}',
    '\u{0364}',
    '\u{0365}',
    '\u{0366}',
    '\u{0367}',
    '\u{0368}',
    '\u{0369}',
    '\u{036A}',
    '\u{036B}',
    '\u{036C}',
    '\u{036D}',
    '\u{036E}',
    '\u{036F}',
    '\u{0483}',
    '\u{0484}',
    '\u{0485}',
    '\u{0486}',
    '\u{0487}',
    '\u{0592}',
    '\u{0593}',
    '\u{0594}',
    '\u{0595}',
    '\u{0597}',
    '\u{0598}',
    '\u{0599}',
    '\u{059C}',
    '\u{059D}',
    '\u{059E}',
    '\u{059F}',
    '\u{05A0}',
    '\u{05A1}',
    '\u{05A8}',
    '\u{05A9}',
    '\u{05AB}',
    '\u{05AC}',
    '\u{05AF}',
    '\u{05C4}',
    '\u{0610}',
    '\u{0611}',
    '\u{0612}',
    '\u{0613}',
    '\u{0614}',
    '\u{0615}',
    '\u{0616}',
    '\u{0617}',
    '\u{0657}',
    '\u{0658}',
    '\u{0659}',
    '\u{065A}',
    '\u{065B}',
    '\u{065D}',
    '\u{065E}',
    '\u{06D6}',
    '\u{06D7}',
    '\u{06D8}',
    '\u{06D9}',
    '\u{06DA}',
    '\u{06DB}',
    '\u{06DC}',
    '\u{06DF}',
    '\u{06E0}',
    '\u{06E1}',
    '\u{06E2}',
    '\u{06E4}',
    '\u{06E7}',
    '\u{06E8}',
    '\u{06EB}',
    '\u{06EC}',
    '\u{0730}',
    '\u{0732}',
    '\u{0733}',
    '\u{0735}',
    '\u{0736}',
    '\u{073A}',
    '\u{073D}',
    '\u{073F}',
    '\u{0740}',
    '\u{0741}',
    '\u{0743}',
    '\u{0745}',
    '\u{0747}',
    '\u{0749}',
    '\u{074A}',
    '\u{07EB}',
    '\u{07EC}',
    '\u{07ED}',
    '\u{07EE}',
    '\u{07EF}',
    '\u{07F0}',
    '\u{07F1}',
    '\u{07F3}',
    '\u{0816}',
    '\u{0817}',
    '\u{0818}',
    '\u{0819}',
    '\u{081B}',
    '\u{081C}',
    '\u{081D}',
    '\u{081E}',
    '\u{081F}',
    '\u{0820}',
    '\u{0821}',
    '\u{0822}',
    '\u{0823}',
    '\u{0825}',
    '\u{0826}',
    '\u{0827}',
    '\u{0829}',
    '\u{082A}',
    '\u{082B}',
    '\u{082C}',
    '\u{082D}',
    '\u{0951}',
    '\u{0953}',
    '\u{0954}',
    '\u{0F82}',
    '\u{0F83}',
    '\u{0F86}',
    '\u{0F87}',
    '\u{135D}',
    '\u{135E}',
    '\u{135F}',
    '\u{17DD}',
    '\u{193A}',
    '\u{1A17}',
    '\u{1A75}',
    '\u{1A76}',
    '\u{1A77}',
    '\u{1A78}',
    '\u{1A79}',
    '\u{1A7A}',
    '\u{1A7B}',
    '\u{1A7C}',
    '\u{1B6B}',
    '\u{1B6D}',
    '\u{1B6E}',
    '\u{1B6F}',
    '\u{1B70}',
    '\u{1B71}',
    '\u{1B72}',
    '\u{1B73}',
    '\u{1CD0}',
    '\u{1CD1}',
    '\u{1CD2}',
    '\u{1CDA}',
    '\u{1CDB}',
    '\u{1CE0}',
    '\u{1DC0}',
    '\u{1DC1}',
    '\u{1DC3}',
    '\u{1DC4}',
    '\u{1DC5}',
    '\u{1DC6}',
    '\u{1DC7}',
    '\u{1DC8}',
    '\u{1DC9}',
    '\u{1DCB}',
    '\u{1DCC}',
    '\u{1DD1}',
    '\u{1DD2}',
    '\u{1DD3}',
    '\u{1DD4}',
    '\u{1DD5}',
    '\u{1DD6}',
    '\u{1DD7}',
    '\u{1DD8}',
    '\u{1DD9}',
    '\u{1DDA}',
    '\u{1DDB}',
    '\u{1DDC}',
    '\u{1DDD}',
    '\u{1DDE}',
    '\u{1DDF}',
    '\u{1DE0}',
    '\u{1DE1}',
    '\u{1DE2}',
    '\u{1DE3}',
    '\u{1DE4}',
    '\u{1DE5}',
    '\u{1DE6}',
    '\u{1DFE}',
    '\u{20D0}',
    '\u{20D1}',
    '\u{20D4}',
    '\u{20D5}',
    '\u{20D6}',
    '\u{20D7}',
    '\u{20DB}',
    '\u{20DC}',
    '\u{20E1}',
    '\u{20E7}',
    '\u{20E9}',
    '\u{20F0}',
    '\u{2CEF}',
    '\u{2CF0}',
    '\u{2CF1}',
    '\u{2DE0}',
    '\u{2DE1}',
    '\u{2DE2}',
    '\u{2DE3}',
    '\u{2DE4}',
    '\u{2DE5}',
    '\u{2DE6}',
    '\u{2DE7}',
    '\u{2DE8}',
    '\u{2DE9}',
    '\u{2DEA}',
    '\u{2DEB}',
    '\u{2DEC}',
    '\u{2DED}',
    '\u{2DEE}',
    '\u{2DEF}',
    '\u{2DF0}',
    '\u{2DF1}',
    '\u{2DF2}',
    '\u{2DF3}',
    '\u{2DF4}',
    '\u{2DF5}',
    '\u{2DF6}',
    '\u{2DF7}',
    '\u{2DF8}',
    '\u{2DF9}',
    '\u{2DFA}',
    '\u{2DFB}',
    '\u{2DFC}',
    '\u{2DFD}',
    '\u{2DFE}',
    '\u{2DFF}',
    '\u{A66F}',
    '\u{A67C}',
    '\u{A67D}',
    '\u{A6F0}',
    '\u{A6F1}',
    '\u{A8E0}',
    '\u{A8E1}',
    '\u{A8E2}',
    '\u{A8E3}',
    '\u{A8E4}',
    '\u{A8E5}',
    '\u{A8E6}',
    '\u{A8E7}',
    '\u{A8E8}',
    '\u{A8E9}',
    '\u{A8EA}',
    '\u{A8EB}',
    '\u{A8EC}',
    '\u{A8ED}',
    '\u{A8EE}',
    '\u{A8EF}',
    '\u{A8F0}',
    '\u{A8F1}',
    '\u{AAB0}',
    '\u{AAB2}',
    '\u{AAB3}',
    '\u{AAB7}',
    '\u{AAB8}',
    '\u{AABE}',
    '\u{AABF}',
    '\u{AAC1}',
    '\u{FE20}',
    '\u{FE21}',
    '\u{FE22}',
    '\u{FE23}',
    '\u{FE24}',
    '\u{FE25}',
    '\u{FE26}',
    '\u{10A0F}',
    '\u{10A38}',
    '\u{1D185}',
    '\u{1D186}',
    '\u{1D187}',
    '\u{1D188}',
    '\u{1D189}',
    '\u{1D1AA}',
    '\u{1D1AB}',
    '\u{1D1AC}',
    '\u{1D1AD}',
    '\u{1D242}',
    '\u{1D243}',
    '\u{1D244}',
];

fn to_base64_str(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

// https://iterm2.com/documentation-images.html
fn iterm2_encode(bytes: &[u8], cell_width: usize, cell_height: usize) -> String {
    format!(
        "\x1b]1337;File=size={};width={};height={};preserveAspectRatio=0;inline=1:{}\u{0007}",
        bytes.len(),
        cell_width,
        cell_height,
        to_base64_str(bytes)
    )
}

// https://sw.kovidgoyal.net/kitty/graphics-protocol/
fn kitty_encode(bytes: &[u8], cell_width: usize, cell_height: usize) -> String {
    let base64_str = to_base64_str(bytes);
    let chunk_size = 4096;

    let mut s = String::new();

    let chunks = base64_str.as_bytes().chunks(chunk_size);
    let total_chunks = chunks.len();

    s.push_str("\x1b_Ga=d,d=C;\x1b\\");
    for (i, chunk) in chunks.enumerate() {
        s.push_str("\x1b_G");
        if i == 0 {
            s.push_str(&format!("a=T,f=100,c={cell_width},r={cell_height},"));
        }
        if i < total_chunks - 1 {
            s.push_str("m=1;");
        } else {
            s.push_str("m=0;");
        }
        s.push_str(std::str::from_utf8(chunk).unwrap());
        s.push_str("\x1b\\");
    }

    s
}

fn kitty_unicode_prepare(
    bytes: &[u8],
    cell_width: usize,
    image_id: u32,
    tmux: bool,
) -> PreparedImage {
    let mut cells = Vec::with_capacity(cell_width);
    let upload_symbol = kitty_unicode_encode(bytes, cell_width, 1, image_id, tmux);
    let foreground = Color::Rgb(
        ((image_id >> 16) & 0xff) as u8,
        ((image_id >> 8) & 0xff) as u8,
        (image_id & 0xff) as u8,
    );
    let image_id_msb = ((image_id >> 24) & 0xff) as usize;

    for column in 0..cell_width {
        let symbol = [
            KITTY_PLACEHOLDER,
            row_column_diacritic(0),
            row_column_diacritic(column),
            row_column_diacritic(image_id_msb),
        ]
        .into_iter()
        .collect();

        cells.push(PreparedImageCell {
            symbol,
            style: Style::default().fg(foreground),
            diff_option: CellDiffOption::ForcedWidth(NonZeroU16::MIN),
        });
    }

    PreparedImage {
        cells,
        upload_data: Some(upload_symbol),
    }
}

fn kitty_unicode_encode(
    bytes: &[u8],
    cell_width: usize,
    cell_height: usize,
    image_id: u32,
    tmux: bool,
) -> String {
    let base64_str = to_base64_str(bytes);
    let chunk_size = 4096;

    let mut s = String::new();

    let chunks = base64_str.as_bytes().chunks(chunk_size);
    let total_chunks = chunks.len();

    let (start, esc, end) = passthrough_escapes(tmux);

    for (i, chunk) in chunks.enumerate() {
        s.push_str(start);
        s.push_str(esc);
        s.push_str("_G");
        if i == 0 {
            s.push_str(&format!(
                "a=T,f=100,U=1,q=2,i={image_id},c={cell_width},r={cell_height},"
            ));
        }
        if i < total_chunks - 1 {
            s.push_str("m=1;");
        } else {
            s.push_str("m=0;");
        }
        s.push_str(std::str::from_utf8(chunk).unwrap());
        s.push_str(esc);
        s.push('\\');
        s.push_str(end);
    }

    s
}

fn row_column_diacritic(index: usize) -> char {
    ROW_COLUMN_DIACRITICS[index]
}

fn kitty_clear_line(y: u16) {
    let y = y + 1; // 1-based
    print!("\x1b_Ga=d,d=Y,y={y};\x1b\\");
}

fn kitty_clear() {
    print!("\x1b_Ga=d,d=A;\x1b\\");
}

fn kitty_unicode_delete_images(image_ids: &[u32], tmux: bool) -> Result<(), io::Error> {
    if image_ids.is_empty() {
        return Ok(());
    }

    let mut stdout = io::stdout().lock();
    for image_id in image_ids {
        write!(
            stdout,
            "{}",
            kitty_unicode_delete_image_encode(*image_id, tmux)
        )?;
    }
    stdout.flush()
}

fn kitty_unicode_delete_image_encode(image_id: u32, tmux: bool) -> String {
    let (start, esc, end) = passthrough_escapes(tmux);
    format!("{start}{esc}_Ga=d,d=I,i={image_id}{esc}\\{end}")
}

fn passthrough_escapes(tmux: bool) -> (&'static str, &'static str, &'static str) {
    if tmux {
        ("\x1bPtmux;", "\x1b\x1b", "\x1b\\")
    } else {
        ("", "\x1b", "")
    }
}

// https://vt100.net/docs/vt3xx-gp/chapter14.html
fn sixel_encode(bytes: &[u8], cell_width: usize, cell_w_px: u32, cell_h_px: u32) -> String {
    let Ok(img) = image::load_from_memory(bytes) else {
        return String::new();
    };
    let width = cell_width as u32 * cell_w_px;
    let height = cell_h_px;
    let img = image::imageops::resize(
        &img.to_rgba8(),
        width,
        height,
        image::imageops::FilterType::Triangle,
    );
    sixel_from_rgba(&img, cell_width)
}

fn sixel_from_rgba(img: &image::RgbaImage, cell_width: usize) -> String {
    let (width, height) = img.dimensions();
    let (palette, indices) = sixel_quantize(img);

    // Save the cursor, erase the covered cells (sixel pixels are not removed by diffing),
    // restore, draw, and restore again so the cursor position does not depend on the terminal.
    let mut s = format!("\x1b7{}\x1b8\x1b7", " ".repeat(cell_width));
    s.push_str("\x1bP0;1;0q");
    s.push_str(&format!("\"1;1;{width};{height}"));
    for (i, [r, g, b]) in palette.iter().enumerate() {
        let pct = |v: u8| (v as u32 * 100 + 127) / 255;
        s.push_str(&format!("#{i};2;{};{};{}", pct(*r), pct(*g), pct(*b)));
    }

    let w = width as usize;
    for band_top in (0..height as usize).step_by(6) {
        let band_h = (height as usize - band_top).min(6);
        // sixel bits per color per column
        let mut used: Vec<u8> = Vec::new();
        let mut columns: Vec<Vec<u8>> = vec![Vec::new(); palette.len()];
        for x in 0..w {
            for dy in 0..band_h {
                let idx = indices[(band_top + dy) * w + x];
                if idx == u8::MAX as u16 + 1 {
                    continue; // transparent
                }
                let c = idx as usize;
                if columns[c].is_empty() {
                    columns[c] = vec![0; w];
                    used.push(c as u8);
                }
                columns[c][x] |= 1 << dy;
            }
        }
        for (n, &c) in used.iter().enumerate() {
            if n > 0 {
                s.push('$');
            }
            s.push_str(&format!("#{c}"));
            sixel_push_rle(&mut s, &columns[c as usize]);
        }
        s.push('-');
    }
    s.push_str("\x1b\\\x1b8");
    s
}

fn sixel_push_rle(s: &mut String, data: &[u8]) {
    let mut i = 0;
    while i < data.len() {
        let v = data[i];
        let mut run = 1;
        while i + run < data.len() && data[i + run] == v {
            run += 1;
        }
        let ch = (v + 63) as char;
        if run > 3 {
            s.push_str(&format!("!{run}{ch}"));
        } else {
            for _ in 0..run {
                s.push(ch);
            }
        }
        i += run;
    }
}

const SIXEL_TRANSPARENT: u16 = 256;

/// Reduces the image to at most 256 colors. Returns the palette and per-pixel palette indices
/// (`SIXEL_TRANSPARENT` for transparent pixels).
fn sixel_quantize(img: &image::RgbaImage) -> (Vec<[u8; 3]>, Vec<u16>) {
    use std::collections::HashMap;

    let mut counts: HashMap<[u8; 3], u32> = HashMap::new();
    for p in img.pixels() {
        if p[3] >= 128 {
            *counts.entry([p[0], p[1], p[2]]).or_default() += 1;
        }
    }

    let palette: Vec<[u8; 3]> = if counts.len() <= 256 {
        let mut v: Vec<_> = counts.keys().copied().collect();
        v.sort_unstable();
        v
    } else {
        median_cut(counts.into_iter().collect(), 256)
    };

    let mut cache: HashMap<[u8; 3], u16> = HashMap::new();
    let indices = img
        .pixels()
        .map(|p| {
            if p[3] < 128 {
                return SIXEL_TRANSPARENT;
            }
            let key = [p[0], p[1], p[2]];
            *cache.entry(key).or_insert_with(|| {
                palette
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, c)| {
                        (0..3)
                            .map(|i| (c[i] as i32 - key[i] as i32).pow(2))
                            .sum::<i32>()
                    })
                    .map(|(i, _)| i as u16)
                    .unwrap_or(0)
            })
        })
        .collect();
    (palette, indices)
}

fn median_cut(colors: Vec<([u8; 3], u32)>, max: usize) -> Vec<[u8; 3]> {
    let mut boxes = vec![colors];
    while boxes.len() < max {
        // split the box with the widest channel range
        let Some((bi, ch, range)) = boxes
            .iter()
            .enumerate()
            .filter(|(_, b)| b.len() > 1)
            .map(|(i, b)| {
                (0..3)
                    .map(|c| {
                        let lo = b.iter().map(|(p, _)| p[c]).min().unwrap();
                        let hi = b.iter().map(|(p, _)| p[c]).max().unwrap();
                        (i, c, hi - lo)
                    })
                    .max_by_key(|t| t.2)
                    .unwrap()
            })
            .max_by_key(|t| t.2)
        else {
            break;
        };
        if range == 0 {
            break;
        }
        let mut b = boxes.swap_remove(bi);
        b.sort_unstable_by_key(|(p, _)| p[ch]);
        let total: u64 = b.iter().map(|(_, n)| *n as u64).sum();
        let mut acc = 0u64;
        let mut split = 1;
        for (i, (_, n)) in b.iter().enumerate() {
            acc += *n as u64;
            if acc * 2 >= total {
                split = (i + 1).min(b.len() - 1);
                break;
            }
        }
        let hi = b.split_off(split);
        boxes.push(b);
        boxes.push(hi);
    }
    boxes
        .iter()
        .map(|b| {
            let total: u64 = b.iter().map(|(_, n)| *n as u64).sum::<u64>().max(1);
            let avg = |c: usize| {
                (b.iter().map(|(p, n)| p[c] as u64 * *n as u64).sum::<u64>() / total) as u8
            };
            [avg(0), avg(1), avg(2)]
        })
        .collect()
}

#[cfg(test)]
mod sixel_tests {
    use super::*;

    #[test]
    fn encodes_two_color_image() {
        let mut img = image::RgbaImage::new(4, 8);
        for (x, _, p) in img.enumerate_pixels_mut() {
            *p = if x < 2 {
                image::Rgba([255, 0, 0, 255])
            } else {
                image::Rgba([0, 0, 0, 0])
            };
        }
        let s = sixel_from_rgba(&img, 1);
        assert!(s.contains("\x1bP0;1;0q\"1;1;4;8#0;2;100;0;0"));
        // two bands (6 + 2 rows); red columns 0..2, transparent columns skipped
        assert!(s.contains("#0~~??-#0BB??-"));
        assert!(s.ends_with("\x1b\\\x1b8"));
    }

    #[test]
    fn quantizes_many_colors_to_256() {
        let mut img = image::RgbaImage::new(64, 64);
        for (x, y, p) in img.enumerate_pixels_mut() {
            *p = image::Rgba([(x * 4) as u8, (y * 4) as u8, ((x + y) * 2) as u8, 255]);
        }
        let (palette, indices) = sixel_quantize(&img);
        assert!(palette.len() <= 256);
        assert!(indices.iter().all(|&i| (i as usize) < palette.len()));
    }
}

#[cfg(test)]
mod cell_size_tests {
    use super::{da1_reports_sixel, is_plausible_cell_size, parse_cell_size_report};

    #[test]
    fn parses_cell_size() {
        assert_eq!(parse_cell_size_report(b"\x1b[6;38;19t"), Some((19, 38)));
        assert_eq!(parse_cell_size_report(b""), None);
        assert!(is_plausible_cell_size((10, 20)));
        assert!(!is_plausible_cell_size((1, 1)));
        assert!(da1_reports_sixel(b"\x1b[?61;4;6;22c"));
        assert!(da1_reports_sixel(b"\x1b[?62;4c"));
        assert!(!da1_reports_sixel(b"\x1b[?62;14c"));
        assert!(!da1_reports_sixel(b"\x1b[?1;2c"));
    }
}
