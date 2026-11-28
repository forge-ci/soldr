#![cfg(feature = "terminal-input")]

use kernal_api::keys::{Key, KeyDecoder, KeyModifiers};

#[test]
fn decoded_keys_preserve_fragments_and_do_not_scan_control_sequences() {
    let mut decoder = KeyDecoder::new();
    assert_eq!(
        decoder.push(b' ').unwrap().unwrap().key,
        Key::Character(' ')
    );
    assert_eq!(decoder.push(b'\r').unwrap().unwrap().key, Key::Enter);
    for byte in b"\x1b[12 " {
        assert!(decoder.push(*byte).unwrap().is_none());
    }
    assert_eq!(decoder.push(b'z').unwrap().unwrap().key, Key::Other);
    assert!(decoder.push(0xc3).unwrap().is_none());
    assert_eq!(
        decoder.push(0xa9).unwrap().unwrap().key,
        Key::Character('é')
    );
    let control = decoder.push(3).unwrap().unwrap();
    assert_eq!(control.key, Key::Character('c'));
    assert_eq!(
        control.modifiers,
        KeyModifiers {
            control: true,
            alt: false
        }
    );
}

#[test]
fn bracketed_paste_and_control_strings_are_not_key_presses() {
    let mut decoder = KeyDecoder::new();
    for byte in b"\x1b[200~hello \r world\x1b[201" {
        assert!(decoder.push(*byte).unwrap().is_none());
    }
    assert_eq!(decoder.push(b'~').unwrap().unwrap().key, Key::Other);
    for byte in b"\x1b]title with spaces" {
        assert!(decoder.push(*byte).unwrap().is_none());
    }
    assert_eq!(decoder.push(7).unwrap().unwrap().key, Key::Other);
}

#[test]
fn malformed_or_oversized_input_fails_closed() {
    let mut decoder = KeyDecoder::new();
    assert!(decoder.push(0xff).is_err());
    assert!(decoder.push(b' ').is_err());
    let mut decoder = KeyDecoder::new();
    for byte in b"\x1b[" {
        decoder.push(*byte).unwrap();
    }
    let mut exceeded = false;
    for _ in 0..129 {
        if decoder.push(b'1').is_err() {
            exceeded = true;
            break;
        }
    }
    assert!(exceeded);
    assert!(decoder.push(b' ').is_err());
}

#[test]
fn function_keys_escape_repeats_and_unsupported_mouse_are_not_text() {
    let mut decoder = KeyDecoder::new();
    for byte in b"\x1bO " {
        assert!(decoder.push(*byte).unwrap().is_none());
    }
    assert_eq!(decoder.push(b'A').unwrap().unwrap().key, Key::Other);
    assert!(decoder.push(27).unwrap().is_none());
    assert_eq!(decoder.push(27).unwrap().unwrap().key, Key::Escape);
    assert_eq!(decoder.finish_pending().unwrap().unwrap().key, Key::Escape);
    for byte in b"\x1b[" {
        decoder.push(*byte).unwrap();
    }
    assert!(decoder.push(b'M').is_err());
    assert!(decoder.push(b' ').is_err());
}

#[test]
fn oversized_paste_and_incomplete_input_poison_the_decoder() {
    let mut decoder = KeyDecoder::new();
    for byte in b"\x1b[200~" {
        decoder.push(*byte).unwrap();
    }
    for _ in 0..65_530 {
        assert!(decoder.push(b' ').unwrap().is_none());
    }
    assert!(decoder.push(b' ').is_err());
    let mut decoder = KeyDecoder::new();
    decoder.push(0xc3).unwrap();
    assert!(decoder.finish_pending().is_err());
    assert!(decoder.push(b' ').is_err());
}

#[test]
fn ambiguous_alt_space_does_not_escape_control_sequence_parsing() {
    let mut decoder = KeyDecoder::new();
    assert!(decoder.push(27).unwrap().is_none());
    assert!(decoder.push(b' ').unwrap().is_none());
    assert!(decoder.finish_pending().is_err());
    assert!(decoder.push(b' ').is_err());
    let mut decoder = KeyDecoder::new();
    decoder.push(27).unwrap();
    let event = decoder.push(b'a').unwrap().unwrap();
    assert_eq!(event.key, Key::Character('a'));
    assert!(event.modifiers.alt);
}
