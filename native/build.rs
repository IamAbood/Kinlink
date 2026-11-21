fn main() {
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/kinlink.ico");
    // File details Windows shows in Properties (an exe without them looks more suspicious).
    res.set("ProductName", "Kinlink");
    res.set("FileDescription", "Kinlink - remote desktop with friends");
    res.set("CompanyName", "Abdulkareem Alsaou");
    res.set("OriginalFilename", "Kinlink.exe");
    res.set("InternalName", "Kinlink");
    res.set("LegalCopyright", "Copyright (c) 2026 Abdulkareem Alsaou");
    res.compile().expect("embed icon");
}
