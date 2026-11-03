# Fedora package; also usable on COPR (enable network access for the build, Cargo
# downloads the Rust libraries).
%global debug_package %{nil}

Name:           mixrail
Version:        0.1.0
Release:        1%{?dist}
Summary:        Linux audio mixer for PipeWire with per-app routing, effects and MIDI control
License:        GPL-3.0-or-later
URL:            https://github.com/thunderthome/mixrail
Source0:        %{url}/archive/v%{version}/%{name}-%{version}.tar.gz

BuildRequires:  cargo >= 1.88
BuildRequires:  rust >= 1.88
BuildRequires:  clang-devel
BuildRequires:  pkgconf
BuildRequires:  pipewire-devel
BuildRequires:  alsa-lib-devel
BuildRequires:  systemd-rpm-macros
Requires:       pipewire
Requires:       pipewire-utils
Requires:       wireplumber

%description
Mixrail is a virtual audio mixer for Linux, an alternative to Voicemeeter on Windows.
Give every app its own channel, mix them onto speakers, headphones and virtual
microphones, add effects like noise suppression, and control it from the window or any
MIDI controller.

%prep
%autosetup

%build
cargo build --release --locked

%check
cargo test --release --locked

%install
install -Dm755 target/release/mixrail %{buildroot}%{_bindir}/mixrail
install -Dm644 packaging/mixrail.service %{buildroot}%{_userunitdir}/mixrail.service
install -Dm644 packaging/mixrail.desktop %{buildroot}%{_datadir}/applications/mixrail.desktop
install -Dm644 assets/icons/mixrail.svg %{buildroot}%{_datadir}/icons/hicolor/scalable/apps/mixrail.svg
install -Dm644 packaging/mixrail-tray.desktop %{buildroot}%{_sysconfdir}/xdg/autostart/mixrail-tray.desktop

%files
%license LICENSE
%doc README.md
%{_bindir}/mixrail
%{_userunitdir}/mixrail.service
%{_datadir}/applications/mixrail.desktop
%{_datadir}/icons/hicolor/scalable/apps/mixrail.svg
%config(noreplace) %{_sysconfdir}/xdg/autostart/mixrail-tray.desktop

%changelog
* Tue Nov 03 2026 thunderthome <47116903+thunderthome@users.noreply.github.com> - 0.1.0-1
- First release
