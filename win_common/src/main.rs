fn main() {
    let parts: Vec<&str> = "screen".split_whitespace().collect();
    match parts.as_slice() {
        ["screen", save_path] => {
            let save_path = save_path.trim_matches('"').to_string();
            let save_path = if save_path.is_empty() {
                "default/path".to_string()  // 替换为你的默认路径
            } else {
                save_path
            };
        }
        _ => unreachable!()
    };
}
