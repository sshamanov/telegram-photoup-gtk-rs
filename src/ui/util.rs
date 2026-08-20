use gdk4::Texture;
use glib::prelude::*;

/// Build a `GdkTexture` from RGBA8 pixels (mpd-client does the same for cover art).
pub fn rgba_to_texture(rgba: &[u8], width: i32, height: i32) -> Texture {
    let bytes = glib::Bytes::from(rgba);
    gdk4::MemoryTexture::new(width, height, gdk4::MemoryFormat::R8g8b8a8, &bytes, (width * 4) as usize)
        .upcast()
}
