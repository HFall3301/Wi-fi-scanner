fn main() {
    let a = wifi_scan::scan();
    let list = match a {
        Ok(a) => a,
        Err(_) => return,
    };
    for a_wifi in list {
        println!("{}", a_wifi);
    }
}
