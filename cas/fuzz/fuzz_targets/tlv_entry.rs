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
        assert_eq!(
            re, data,
            "decoder accepted a non-canonical entry encoding"
        );
    }
});
