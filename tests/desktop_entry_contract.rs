#[test]
fn production_desktop_entry_matches_the_window_to_its_icon_and_binary() {
    let desktop = include_str!("../packaging/dev.cagents.Cagents.desktop");
    assert!(
        desktop.contains("Exec=cagents ui\n"),
        "the menu must launch through the public CLI"
    );
    assert!(
        desktop.contains("TryExec=cagents\n"),
        "application menus hide a missing binary instead of offering a dead entry"
    );
    assert!(
        desktop.contains("Icon=dev.cagents.Cagents\n"),
        "the icon name must match the installed hicolor file"
    );
    assert!(
        desktop.contains("StartupWMClass=dev.cagents.Cagents\n"),
        "compositors match the window to the desktop icon through the application id"
    );
}

#[test]
fn installer_stamps_an_absolute_menu_exec_and_icon() {
    let installer = include_str!("../install.sh");
    assert!(
        installer.contains("Exec=\"%s\" ui"),
        "the installed desktop entry must not depend on PATH"
    );
    assert!(
        installer.contains("TryExec=%s"),
        "the installed desktop entry must hide itself when the binary is gone"
    );
    assert!(
        installer.contains("Icon=%s"),
        "the installed desktop entry must name the packaged icon by path"
    );
    assert!(
        installer.contains("share/icons/hicolor/scalable/apps/dev.cagents.Cagents.svg"),
        "the stamped icon path must stay on the current install symlink"
    );
}
