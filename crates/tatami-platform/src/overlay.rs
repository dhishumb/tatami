//! The which-key sheet: one borderless, non-activating `NSPanel` holding a
//! single label. Layout is plain monospaced text built by `compose`, so the
//! AppKit side is one attributed string and one frame.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSFont, NSFontAttributeName, NSFontWeightBold,
    NSFontWeightRegular, NSForegroundColorAttributeName, NSPanel, NSStatusWindowLevel, NSTextField,
    NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_core_foundation::{CGFloat, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGDisplayBounds, CGMainDisplayID};
use objc2_foundation::{
    NSAttributedString, NSAttributedStringKey, NSDictionary, NSMutableAttributedString, NSString,
};
use tatami_core::{MonitorInfo, OverlayContent};

const FONT_SIZE: CGFloat = 13.0;
const PAD: CGFloat = 12.0;
const BOTTOM_GAP: CGFloat = 24.0;
const ARROW: &str = " → ";
const GAP: usize = 3;
const DESC_MAX: usize = 28;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Style {
    Title,
    Dim,
    Key,
    Desc,
    Prefix,
}

/// (r, g, b) per style, in `Style` order.
const COLORS: [(f64, f64, f64); 5] = [
    (0.45, 0.72, 1.0),
    (0.55, 0.55, 0.60),
    (0.45, 0.72, 1.0),
    (0.90, 0.90, 0.92),
    (0.86, 0.52, 0.96),
];

/// Columns of item indices, filled top to bottom then left to right, in at
/// most `max_cols` columns of at most `max_rows` rows; overflow is dropped.
pub(crate) fn pack(n: usize, max_cols: usize, max_rows: usize) -> Vec<Vec<usize>> {
    if n == 0 {
        return Vec::new();
    }
    let cols = max_cols.max(1);
    let rows = n.div_ceil(cols).clamp(1, max_rows.max(1));
    let idx: Vec<usize> = (0..n.min(rows * cols)).collect();
    idx.chunks(rows).map(<[usize]>::to_vec).collect()
}

fn width(s: &str) -> usize {
    s.chars().count()
}

fn truncate(s: &str, max: usize) -> String {
    if width(s) <= max {
        return s.to_owned();
    }
    let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
    t.push('…');
    t
}

#[derive(Default)]
struct Runs(Vec<(Style, String)>);

impl Runs {
    fn push(&mut self, style: Style, s: &str) {
        match self.0.last_mut() {
            Some((st, t)) if *st == style => t.push_str(s),
            _ => self.0.push((style, s.to_owned())),
        }
    }

    /// Spaces carry no colour, so they join whatever run is open.
    fn pad(&mut self, n: usize) {
        match self.0.last_mut() {
            Some((_, t)) => t.extend(std::iter::repeat_n(' ', n)),
            None => self.0.push((Style::Dim, " ".repeat(n))),
        }
    }
}

/// The sheet as styled text, `cols_chars` wide at most, body at most `max_rows` lines.
pub(crate) fn compose(
    c: &OverlayContent,
    cols_chars: usize,
    max_rows: usize,
) -> Vec<(Style, String)> {
    let key_w = c.rows.iter().map(|r| width(&r.key)).max().unwrap_or(0);
    let desc_w = c
        .rows
        .iter()
        .map(|r| width(&r.desc))
        .max()
        .unwrap_or(0)
        .min(c.max_desc.unwrap_or(DESC_MAX));
    let cell = key_w + width(ARROW) + desc_w;
    let fit = (cols_chars + GAP) / (cell + GAP);
    let fit = c.columns.map_or(fit, |n| n.min(fit).max(1));
    let cols = pack(c.rows.len(), fit, max_rows);
    let body_w = (cols.len() * (cell + GAP)).saturating_sub(GAP);

    let mut out = Runs::default();
    // Header: title left, exit hint right, title truncated first.
    let hint_w = width(&c.exit_hint);
    let line_w = body_w
        .max(width(&c.title) + 2 + hint_w)
        .min(cols_chars.max(hint_w + 3));
    let title = truncate(&c.title, line_w.saturating_sub(hint_w + 2));
    out.push(Style::Title, &title);
    out.pad(line_w - width(&title) - hint_w);
    out.push(Style::Dim, &c.exit_hint);

    let nrows = cols.first().map_or(0, Vec::len);
    for r in 0..nrows {
        out.push(Style::Dim, "\n");
        for (ci, col) in cols.iter().enumerate() {
            let Some(&i) = col.get(r) else { break };
            let row = &c.rows[i];
            if ci > 0 {
                out.pad(GAP);
            }
            out.pad(key_w - width(&row.key));
            out.push(Style::Key, &row.key);
            out.push(Style::Dim, ARROW);
            let desc = truncate(&row.desc, c.max_desc.unwrap_or(DESC_MAX));
            out.push(
                if row.prefix {
                    Style::Prefix
                } else {
                    Style::Desc
                },
                &desc,
            );
            if cols.get(ci + 1).is_some_and(|next| next.len() > r) {
                out.pad(desc_w - width(&desc));
            }
        }
    }
    out.0
}

type Attrs = Retained<NSDictionary<NSAttributedStringKey, AnyObject>>;

struct Sheet {
    panel: Retained<NSPanel>,
    label: Retained<NSTextField>,
    font: Retained<NSFont>,
    attrs: [Attrs; 5],
}

#[derive(Default)]
pub(crate) struct Overlay {
    sheet: Option<Sheet>,
    last: Option<(OverlayContent, MonitorInfo)>,
}

impl Overlay {
    pub(crate) fn show(
        &mut self,
        mtm: MainThreadMarker,
        content: Option<&OverlayContent>,
        monitor: &MonitorInfo,
    ) {
        let Some(content) = content else {
            if self.last.take().is_some()
                && let Some(s) = &self.sheet
            {
                s.panel.orderOut(None);
            }
            return;
        };
        if self
            .last
            .as_ref()
            .is_some_and(|(c, m)| c == content && m == monitor)
        {
            return;
        }
        let sheet = self.sheet.get_or_insert_with(|| Sheet::new(mtm));
        sheet.render(content, monitor);
        self.last = Some((content.clone(), monitor.clone()));
    }
}

fn attrs(font: &NSFont, (r, g, b): (f64, f64, f64)) -> Attrs {
    let color = NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, 1.0);
    // SAFETY: reading immutable framework constants.
    let keys = unsafe { [NSFontAttributeName, NSForegroundColorAttributeName] };
    let vals: [&AnyObject; 2] = [font.as_ref(), color.as_ref()];
    NSDictionary::from_slices(&keys, &vals)
}

impl Sheet {
    fn new(mtm: MainThreadMarker) -> Self {
        let frame = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(10.0, 10.0));
        let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            frame,
            NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
            NSBackingStoreType::Buffered,
            false,
        );
        panel.setLevel(NSStatusWindowLevel + 1);
        panel.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
        panel.setIgnoresMouseEvents(true);
        panel.setHasShadow(true);
        panel.setOpaque(false);
        // Our app is never active; the default would hide the panel at once.
        panel.setHidesOnDeactivate(false);
        panel.setBackgroundColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
            0.11, 0.11, 0.13, 0.94,
        )));
        // SAFETY: we hold the only strong reference; AppKit must not release it on close.
        unsafe { panel.setReleasedWhenClosed(false) };

        let label = NSTextField::labelWithAttributedString(&NSAttributedString::new(), mtm);
        label.setEditable(false);
        label.setSelectable(false);
        label.setBezeled(false);
        label.setDrawsBackground(false);
        if let Some(view) = panel.contentView() {
            view.addSubview(&label);
        }

        // SAFETY: reading immutable framework constants.
        let (regular, bold) = unsafe { (NSFontWeightRegular, NSFontWeightBold) };
        let font = NSFont::monospacedSystemFontOfSize_weight(FONT_SIZE, regular);
        let bold = NSFont::monospacedSystemFontOfSize_weight(FONT_SIZE, bold);
        let attrs = [0, 1, 2, 3, 4].map(|i| {
            attrs(
                if i == Style::Title as usize {
                    &bold
                } else {
                    &font
                },
                COLORS[i],
            )
        });
        Self {
            panel,
            label,
            font,
            attrs,
        }
    }

    fn render(&self, content: &OverlayContent, monitor: &MonitorInfo) {
        let vis = monitor.visible;
        let char_w = self.font.maximumAdvancement().width.max(1.0);
        let line_h = self.font.ascender() - self.font.descender() + self.font.leading();
        let cols_chars = ((vis.w * 0.9 - 2.0 * PAD) / char_w).max(1.0) as usize;
        // A third of the screen, minus the header line.
        let max_rows = ((vis.h / 3.0 / line_h.max(1.0)) as usize).saturating_sub(1);

        let text = NSMutableAttributedString::new();
        for (style, s) in compose(content, cols_chars, max_rows) {
            // SAFETY: the attributes map NSAttributedStringKeys to an NSFont and an NSColor,
            // the value types those keys require.
            let run = unsafe {
                NSAttributedString::initWithString_attributes(
                    NSAttributedString::alloc(),
                    &NSString::from_str(&s),
                    Some(&self.attrs[style as usize]),
                )
            };
            text.appendAttributedString(&run);
        }
        self.label.setAttributedStringValue(&text);
        self.label.sizeToFit();
        let size = self.label.frame().size;
        self.label.setFrameOrigin(CGPoint::new(PAD, PAD));

        let (w, h) = (size.width + 2.0 * PAD, size.height + 2.0 * PAD);
        // AX frames are top-left based; AppKit's origin is the primary display's bottom-left.
        let main_h = CGDisplayBounds(CGMainDisplayID()).size.height;
        let x = vis.x + (vis.w - w) / 2.0;
        let y = main_h - (vis.y + vis.h) + BOTTOM_GAP;
        self.panel
            .setFrame_display(CGRect::new(CGPoint::new(x, y), CGSize::new(w, h)), true);
        self.panel.orderFrontRegardless();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tatami_core::OverlayRow;

    #[test]
    fn pack_fills_columns_top_to_bottom() {
        assert!(pack(0, 4, 10).is_empty());
        assert_eq!(pack(5, 2, 10), vec![vec![0, 1, 2], vec![3, 4]]);
        assert_eq!(
            pack(14, 6, 10),
            vec![
                vec![0, 1, 2],
                vec![3, 4, 5],
                vec![6, 7, 8],
                vec![9, 10, 11],
                vec![12, 13]
            ]
        );
        // Row cap drops the overflow; zero fit still yields one column.
        assert_eq!(pack(7, 2, 2), vec![vec![0, 1], vec![2, 3]]);
        assert_eq!(pack(3, 0, 10), vec![vec![0, 1, 2]]);
    }

    fn row(key: &str, desc: &str, prefix: bool) -> OverlayRow {
        OverlayRow {
            key: key.into(),
            desc: desc.into(),
            prefix,
        }
    }

    fn plain(runs: &[(Style, String)]) -> String {
        runs.iter().map(|(_, s)| s.as_str()).collect()
    }

    #[test]
    fn compose_aligns_keys_and_descs() {
        let c = OverlayContent {
            title: "⌥".into(),
            exit_hint: "Esc  back".into(),
            rows: vec![
                row("h", "focus left", false),
                row("tab", "workspace back and forth", false),
                row("r", "+resize", true),
            ],
            columns: None,
            max_desc: None,
        };
        // Cell = 3 + 3 + 24 = 30; 63 chars fit two columns.
        let runs = compose(&c, 63, 10);
        let text = plain(&runs);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with('⌥') && lines[0].ends_with("Esc  back"));
        assert_eq!(width(lines[0]), 63);
        // "focus left" padded to 24, the 3-space gap, then "r" right-aligned to 3.
        let pad = " ".repeat(24 - 10 + 3 + 2);
        assert_eq!(lines[1], format!("  h → focus left{pad}r → +resize"));
        assert_eq!(lines[2], "tab → workspace back and forth");
        assert!(runs.contains(&(Style::Prefix, "+resize".into())));
    }

    #[test]
    fn compose_truncates_long_descs() {
        let c = OverlayContent {
            title: "t".into(),
            exit_hint: String::new(),
            rows: vec![row("a", &"x".repeat(40), false)],
            columns: None,
            max_desc: None,
        };
        let text = plain(&compose(&c, 200, 10));
        let body = text.lines().nth(1).unwrap();
        assert_eq!(width(body), 1 + 3 + DESC_MAX);
        assert!(body.ends_with('…'));
    }
}
