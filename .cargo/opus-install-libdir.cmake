# Used as CMAKE_TOOLCHAIN_FILE for the libopus build of audiopus_sys: it
# expects the library in <out>/lib, but GNUInstallDirs picks lib64 on some
# distributions (Fedora, NixOS, Arch…).
set(CMAKE_INSTALL_LIBDIR lib CACHE PATH "Install libraries to lib/" FORCE)
