fn main() {
    let _ = embed_resource::compile("capslock-switcher.rc", embed_resource::NONE)
        .manifest_optional();
}
