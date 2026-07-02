// Permission to use, copy, modify, and/or distribute this software for
// any purpose with or without fee is hereby granted.
//
// THE SOFTWARE IS PROVIDED “AS IS” AND THE AUTHOR DISCLAIMS ALL
// WARRANTIES WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES
// OF MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE
// FOR ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY
// DAMAGES WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN
// AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT
// OF OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.

#![no_main]
//! The workhorse oracle: directory-entry TLV is canonical, so any bytes
//! the decoder accepts must equal their own re-encoding. A decoder that
//! tolerates non-canonical bytes (unsorted optional tags, an absent field
//! spelled as a zero, slack length) is panic-free yet still a bug — it
//! makes two byte strings denote one logical entry, silently breaking
//! "same contents ⇒ same hash," the invariant the whole store rests on.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(entry) = cas::tlv::decode(data) {
        let re = cas::tlv::encode(&entry);
        assert_eq!(re, data, "decoder accepted a non-canonical entry encoding");
    }
});
