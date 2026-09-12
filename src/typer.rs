// SPDX-License-Identifier: MPL-2.0

//! Types a transcript into whatever window currently has keyboard focus,
//! using `zwp_virtual_keyboard_v1`.
//!
//! There is no Wayland request for "insert this string", so the approach is
//! the one `wtype` uses: build a throwaway XKB keymap in which each character
//! of the text gets its own one-level key, hand that keymap to the
//! compositor, then press and release those keys. The keymap is capped at 255
//! keycodes, so long text is typed in several passes.

use std::io::Write;
use std::os::fd::AsFd;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry::WlRegistry, wl_seat::WlSeat};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};

/// `wl_keyboard::keymap_format::xkb_v1`; the protocol takes it as a raw uint.
const KEYMAP_FORMAT_XKB_V1: u32 = 1;
/// XKB keycodes run 8..=255, and evdev keycode 0 is `KEY_RESERVED`, so the
/// usable range starts at evdev 1 (XKB 9). Leave headroom under the ceiling.
const FIRST_EVDEV_CODE: u32 = 1;
const MAX_KEYS: usize = 200;

/// Empty marker state: none of the objects used here emit events we act on.
struct State;

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as wayland_client::Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

macro_rules! ignore_events {
    ($($proxy:ty),+ $(,)?) => {
        $(impl Dispatch<$proxy, ()> for State {
            fn event(
                _: &mut Self,
                _: &$proxy,
                _: <$proxy as wayland_client::Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        })+
    };
}

ignore_events!(WlSeat, ZwpVirtualKeyboardManagerV1, ZwpVirtualKeyboardV1);

/// True if the compositor advertises the virtual-keyboard protocol at all.
/// Used to warn in the popup before the user records something they cannot
/// get back.
pub fn is_available() -> bool {
    let Ok(connection) = Connection::connect_to_env() else {
        return false;
    };
    let Ok((globals, _queue)) = registry_queue_init::<State>(&connection) else {
        return false;
    };
    globals.contents().with_list(|globals| {
        globals
            .iter()
            .any(|global| global.interface == "zwp_virtual_keyboard_manager_v1")
    })
}

/// Type `text` into the focused window. Blocking; call it off the UI thread.
///
/// `delay` is held between characters — zero is fine for most toolkits, but
/// some applications drop keys that arrive in the same millisecond.
pub fn type_text(text: &str, delay: Duration) -> Result<()> {
    if text.is_empty() {
        return Ok(());
    }

    let connection =
        Connection::connect_to_env().context("cannot connect to the Wayland display")?;
    let (globals, mut queue) =
        registry_queue_init::<State>(&connection).context("cannot read the Wayland registry")?;
    let qh = queue.handle();

    let manager: ZwpVirtualKeyboardManagerV1 = globals.bind(&qh, 1..=1, ()).map_err(|error| {
        anyhow!(
            "this compositor does not offer zwp_virtual_keyboard_manager_v1 ({error}); \
                 the transcript was not typed"
        )
    })?;
    let seat: WlSeat = globals
        .bind(&qh, 1..=9, ())
        .map_err(|error| anyhow!("no Wayland seat available ({error})"))?;

    let keyboard = manager.create_virtual_keyboard(&seat, &qh, ());
    let mut clock = Clock::new(delay);

    for chunk in chunk_by_unique_chars(text, MAX_KEYS) {
        let layout = Layout::build(chunk);
        upload_keymap(&keyboard, &layout.keymap_source())?;
        // Let the compositor load the keymap before keys referencing it.
        queue
            .roundtrip(&mut State)
            .context("keymap was not accepted")?;

        // Start from a clean modifier state so Caps Lock or a held Ctrl on
        // the physical keyboard cannot alter what we send.
        keyboard.modifiers(0, 0, 0, 0);

        for character in chunk.chars() {
            let Some(code) = layout.evdev_code(character) else {
                continue;
            };
            keyboard.key(clock.tick(), code, 1);
            keyboard.key(clock.tick(), code, 0);
            connection.flush().context("cannot flush key events")?;
            if !delay.is_zero() {
                std::thread::sleep(delay);
            }
        }

        queue
            .roundtrip(&mut State)
            .context("key events were not delivered")?;
    }

    keyboard.destroy();
    connection
        .flush()
        .context("cannot flush the Wayland connection")?;
    Ok(())
}

/// Monotonic millisecond timestamps; the protocol requires them to increase.
struct Clock {
    now: u32,
    step: u32,
}

impl Clock {
    fn new(delay: Duration) -> Self {
        Self {
            now: 0,
            step: (delay.as_millis() as u32).max(1),
        }
    }

    fn tick(&mut self) -> u32 {
        self.now = self.now.wrapping_add(self.step);
        self.now
    }
}

/// One keymap's worth of characters: the unique characters of a chunk, each
/// assigned an evdev keycode.
struct Layout {
    characters: Vec<char>,
}

impl Layout {
    fn build(chunk: &str) -> Self {
        let mut characters = Vec::new();
        for character in chunk.chars() {
            if !characters.contains(&character) {
                characters.push(character);
            }
        }
        Self { characters }
    }

    fn evdev_code(&self, character: char) -> Option<u32> {
        self.characters
            .iter()
            .position(|candidate| *candidate == character)
            .map(|index| FIRST_EVDEV_CODE + index as u32)
    }

    fn keymap_source(&self) -> String {
        let last_xkb_code = FIRST_EVDEV_CODE as usize + self.characters.len() - 1 + 8;

        let mut keycodes = String::new();
        let mut symbols = String::new();
        for (index, character) in self.characters.iter().enumerate() {
            let evdev = FIRST_EVDEV_CODE as usize + index;
            keycodes.push_str(&format!("    <K{evdev}> = {};\n", evdev + 8));
            // ONE_LEVEL keeps the compositor from applying shift or caps
            // rules to a key that already carries the exact symbol we want.
            symbols.push_str(&format!(
                "    key <K{evdev}> {{ type[Group1] = \"ONE_LEVEL\", [ {} ] }};\n",
                keysym_name(*character)
            ));
        }

        format!(
            "xkb_keymap {{\n\
             xkb_keycodes \"whispr\" {{\n    minimum = 8;\n    maximum = {last_xkb_code};\n{keycodes}}};\n\
             xkb_types \"whispr\" {{ include \"complete\" }};\n\
             xkb_compatibility \"whispr\" {{ include \"complete\" }};\n\
             xkb_symbols \"whispr\" {{\n    name[Group1] = \"Whispr\";\n{symbols}}};\n\
             }};\n"
        )
    }
}

/// XKB spells most characters as `U` plus the hex codepoint. The handful of
/// control characters that can appear in a transcript have proper names.
fn keysym_name(character: char) -> String {
    match character {
        '\n' => "Return".to_string(),
        '\r' => "Return".to_string(),
        '\t' => "Tab".to_string(),
        other => format!("U{:04X}", other as u32),
    }
}

/// Split text so that no piece needs more than `limit` distinct characters,
/// since one keymap cannot hold more than that.
fn chunk_by_unique_chars(text: &str, limit: usize) -> Vec<&str> {
    let mut chunks = Vec::new();
    let mut seen: Vec<char> = Vec::new();
    let mut start = 0;

    for (offset, character) in text.char_indices() {
        if !seen.contains(&character) {
            if seen.len() == limit {
                chunks.push(&text[start..offset]);
                start = offset;
                seen.clear();
            }
            seen.push(character);
        }
    }

    if start < text.len() {
        chunks.push(&text[start..]);
    }
    chunks
}

/// Hand the keymap to the compositor through a memfd, as the protocol
/// requires: a NUL-terminated string the compositor maps read-only.
fn upload_keymap(keyboard: &ZwpVirtualKeyboardV1, source: &str) -> Result<()> {
    let fd = rustix::fs::memfd_create("cosmic-whispr-keymap", rustix::fs::MemfdFlags::CLOEXEC)
        .context("cannot create the keymap memfd")?;
    let mut file = std::fs::File::from(fd);
    file.write_all(source.as_bytes())
        .context("cannot write the keymap")?;
    file.write_all(&[0])
        .context("cannot terminate the keymap")?;
    file.flush().context("cannot flush the keymap")?;

    let size = source.len() + 1;
    keyboard.keymap(KEYMAP_FORMAT_XKB_V1, file.as_fd(), size as u32);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keysym_names_cover_ascii_and_unicode() {
        assert_eq!(keysym_name('A'), "U0041");
        assert_eq!(keysym_name(' '), "U0020");
        assert_eq!(keysym_name('é'), "U00E9");
        assert_eq!(keysym_name('😀'), "U1F600");
        assert_eq!(keysym_name('\n'), "Return");
        assert_eq!(keysym_name('\t'), "Tab");
    }

    #[test]
    fn layout_assigns_one_code_per_unique_char() {
        let layout = Layout::build("hello");
        assert_eq!(layout.characters, vec!['h', 'e', 'l', 'o']);
        assert_eq!(layout.evdev_code('h'), Some(1));
        assert_eq!(layout.evdev_code('o'), Some(4));
        assert_eq!(layout.evdev_code('z'), None);
    }

    #[test]
    fn keymap_source_stays_within_the_xkb_range() {
        let text: String = (0..MAX_KEYS)
            .map(|n| char::from_u32(0x100 + n as u32).unwrap())
            .collect();
        let source = Layout::build(&text).keymap_source();

        let maximum: usize = source
            .lines()
            .find_map(|line| {
                line.trim()
                    .strip_prefix("maximum = ")?
                    .trim_end_matches(';')
                    .parse()
                    .ok()
            })
            .expect("keymap declares a maximum");
        assert!(
            maximum <= 255,
            "maximum keycode {maximum} exceeds the XKB limit"
        );
        assert!(source.contains("<K1> = 9;"));
        assert!(source.contains("ONE_LEVEL"));
    }

    #[test]
    fn chunking_respects_the_keymap_limit() {
        let text: String = (0..500)
            .map(|n| char::from_u32(0x100 + n as u32).unwrap())
            .collect();
        let chunks = chunk_by_unique_chars(&text, MAX_KEYS);

        assert_eq!(chunks.concat(), text, "chunking must not lose characters");
        for chunk in &chunks {
            let unique = Layout::build(chunk).characters.len();
            assert!(unique <= MAX_KEYS, "chunk needs {unique} keycodes");
        }
    }

    #[test]
    fn repeated_characters_do_not_force_new_chunks() {
        let text = "ab".repeat(1_000);
        assert_eq!(chunk_by_unique_chars(&text, MAX_KEYS).len(), 1);
    }

    #[test]
    fn chunking_handles_short_and_empty_text() {
        assert_eq!(chunk_by_unique_chars("hi", MAX_KEYS), vec!["hi"]);
        assert!(chunk_by_unique_chars("", MAX_KEYS).is_empty());
    }
}
