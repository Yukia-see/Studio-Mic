Name:           studio-mic
# install.sh passes app_version (from Cargo.toml) and build_number (packaging/BUILD_NUMBER).
Version:        %{?app_version}%{!?app_version:0.2.0}
Release:        %{?build_number}%{!?build_number:1}%{?dist}
Summary:        Broadcast microphone processing and virtual mixer for PipeWire
License:        Apache-2.0 OR MIT
URL:            https://github.com/studiomic/studiomic
Source0:        %{name}-%{version}.tar.gz
ExclusiveArch:  x86_64

BuildRequires:  cargo rust gcc pkgconfig(webkit2gtk-4.1) pkgconfig(gtk+-3.0) pkgconfig(speexdsp) curl unzip
Requires:       pipewire wireplumber pipewire-pulseaudio webkit2gtk4.1 gtk3 ladspa lsp-plugins-ladspa speexdsp
Recommends:     ladspa-swh-plugins obs-studio
Provides:       studiomic = %{version}-%{release}

# Bundled prebuilt LADSPA plugins carry their own symbols.
%global debug_package %{nil}
%{!?_userunitdir: %global _userunitdir %{_prefix}/lib/systemd/user}

%description
Studio Mic adds broadcast processing (noise removal, smart gate, de-esser, 10-band EQ,
compressor, Clipguard) to your microphone and a stream/VOD/monitor mixer to PipeWire.
Four persistent buses are exposed: Voice Out (Discord), Stream Mix, VOD Mix and Monitor Mix.

%prep
%autosetup -n %{name}-%{version}
./setup-deps.sh --no-packages --stage ladspa-staging -y

%build
cargo build --release --locked
# Studio Mic's own Speex LADSPA wrapper (no distro packages one).
gcc %{optflags} -fPIC -shared -o libspeex_ladspa.so plugins/speex-ladspa/speex_ladspa.c $(pkg-config --cflags --libs speexdsp) %{build_ldflags}

%install
install -Dm755 target/release/StudioMic-Package %{buildroot}%{_bindir}/studiomic
ln -s studiomic %{buildroot}%{_bindir}/studio-mic
install -Dm644 studiomic.desktop %{buildroot}%{_datadir}/applications/studiomic.desktop
install -Dm644 assets/studiomic.svg %{buildroot}%{_datadir}/icons/hicolor/scalable/apps/studiomic.svg
install -Dm644 studiomic.service %{buildroot}%{_userunitdir}/studiomic.service
install -Dm644 pipewire-conf/60-studio-mic-buses.conf %{buildroot}%{_datadir}/pipewire/pipewire.conf.d/60-studio-mic-buses.conf
install -Dm755 ladspa-staging/libdeep_filter_ladspa.so %{buildroot}%{_libdir}/ladspa/libdeep_filter_ladspa.so
install -Dm755 ladspa-staging/librnnoise_ladspa.so %{buildroot}%{_libdir}/ladspa/librnnoise_ladspa.so
install -Dm755 libspeex_ladspa.so %{buildroot}%{_libdir}/ladspa/libspeex_ladspa.so
mkdir -p %{buildroot}%{_datadir}/studio-mic
cp -r streamcontroller-plugin %{buildroot}%{_datadir}/studio-mic/
find %{buildroot}%{_datadir}/studio-mic -name __pycache__ -type d -prune -exec rm -rf {} +

%preun
# $1 == 0: complete removal (not an upgrade). Clean up for every logged-in user.
if [ "$1" -eq 0 ]; then
    for uid in $(loginctl list-users --no-legend 2>/dev/null | awk '{print $1}'); do
        user="$(getent passwd "$uid" | cut -d: -f1)"
        [ -n "$user" ] || continue
        runuser -u "$user" -- env XDG_RUNTIME_DIR="/run/user/$uid" %{_bindir}/studiomic --cleanup-pipewire >/dev/null 2>&1 || :
    done
fi

%files
%{_bindir}/studiomic
%{_bindir}/studio-mic
%{_datadir}/applications/studiomic.desktop
%{_datadir}/icons/hicolor/scalable/apps/studiomic.svg
%{_userunitdir}/studiomic.service
%{_datadir}/pipewire/pipewire.conf.d/60-studio-mic-buses.conf
%{_libdir}/ladspa/libdeep_filter_ladspa.so
%{_libdir}/ladspa/librnnoise_ladspa.so
%{_libdir}/ladspa/libspeex_ladspa.so
%{_datadir}/studio-mic/

%changelog
* Mon Nov 02 2026 Studio Mic Project - 0.2.0-1
- Two-tier persistent bus architecture, live parameter updates, state persistence, CLI socket.
