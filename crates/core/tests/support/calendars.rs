pub(crate) fn feed(date: &str) -> String {
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:trip\r\nDTSTART:{date}\r\nSUMMARY:Trip\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
    )
}
