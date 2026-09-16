//! Card container shared by the app's pages.
//!
//! These cards used to be hand-drawn Cairo "tech panels": cut corners, an
//! accent frame, a corner bracket at each cut, a gradient tint. All of that
//! is gone - `build`/`build_simple` now return a plain libadwaita card
//! (`.card`), with the same fields and signatures, so none of the pages that
//! use them had to change.
//!
//! The one runtime knob, `set_accent`, kept its meaning too: callers pass the
//! brand's bright accent for "selected" and its dark pair for "not", so the
//! selection state still works - it is a CSS class on `.accent-selected` in
//! `resources/style.css` now, instead of recoloring a `DrawingArea`.

use gtk4 as gtk;
use gtk4::prelude::*;

#[derive(Clone)]
pub struct FacetedCard {
    /// Place this in your layout.
    pub widget: gtk::Widget,
    /// Append your own content into this.
    pub content: gtk::Box,
}

impl FacetedCard {
    /// Marks the card selected or not, from the accent the caller passed
    /// (`brand_theme::accent().bright` for active, `.dark` otherwise - see
    /// `fan_page.rs`). Colors are compared with a small tolerance rather than
    /// for exact equality: they travel as `f64` triples through the pages'
    /// own state, and the bright brand value is the only thing any caller
    /// passes that should read as "selected".
    pub fn set_accent(&self, accent: (f64, f64, f64)) {
        let bright = crate::ui::brand_theme::accent().bright;
        let is_bright = (accent.0 - bright.0).abs() < 0.01
            && (accent.1 - bright.1).abs() < 0.01
            && (accent.2 - bright.2).abs() < 0.01;
        if is_bright {
            self.widget.add_css_class("accent-selected");
        } else {
            self.widget.remove_css_class("accent-selected");
        }
    }
}

fn new_card(title: Option<&str>, compact: bool) -> FacetedCard {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 4);
    root.add_css_class("card");

    if let Some(text) = title {
        let label = gtk::Label::new(Some(text));
        label.add_css_class("card-title");
        label.set_halign(gtk::Align::Start);
        root.append(&label);
    }

    let content = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    content.set_hexpand(true);
    content.set_vexpand(true);
    // Padding is margins on the content box, not CSS `padding` on `.card`:
    // `.card` is a class libadwaita itself styles elsewhere, and a global
    // padding rule here would have reached those widgets too.
    let pad = if compact { 12 } else { 14 };
    content.set_margin_top(if title.is_some() { 6 } else { pad });
    content.set_margin_bottom(pad);
    content.set_margin_start(pad);
    content.set_margin_end(pad);
    root.append(&content);

    FacetedCard {
        widget: root.upcast(),
        content,
    }
}

/// Builds an empty card. `accent` is ignored: an Adwaita card has one
/// theme-driven color. The parameter stays so the pages that already pass it
/// don't have to change.
pub fn build(_accent: (f64, f64, f64), title: Option<&str>) -> FacetedCard {
    new_card(title, false)
}

/// [`build`] with the tighter padding compact cards used.
pub fn build_simple(_accent: (f64, f64, f64), title: Option<&str>) -> FacetedCard {
    new_card(title, true)
}
