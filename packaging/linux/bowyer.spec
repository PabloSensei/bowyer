# The .rpm of Bowyer, made by repackage.sh from the files of the .deb (@VERSION@ is filled in).
%global debug_package %{nil}
%global __os_install_post %{nil}

Name:           bowyer
Version:        @VERSION@
Release:        1
Summary:        One window for every AI coding harness, local or over SSH
License:        GPL-3.0-or-later
URL:            https://github.com/PabloSensei/bowyer
Source0:        data.tar
ExclusiveArch:  x86_64
# Opened at start, not linked: rpmbuild does not see them.
Requires:       libwayland-client.so.0()(64bit)
Requires:       libwayland-egl.so.1()(64bit)
Requires:       libEGL.so.1()(64bit)

%description
Bowyer shows the sessions of Claude Code, Codex, OpenCode and DeepSeek on this computer and
on hosts reached over SSH in one tree, renders their history, runs new turns and keeps its
own memory.

%prep

%build

%install
mkdir -p %{buildroot}
tar -xf %{SOURCE0} -C %{buildroot}

%files
/usr/bin/bowyer
/usr/share/applications/bowyer.desktop
/usr/share/icons/hicolor/*/apps/bowyer.png
/usr/lib/bowyer
