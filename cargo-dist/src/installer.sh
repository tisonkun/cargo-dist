#!/bin/bash
#
# Licensed under the MIT license
# <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
# option. This file may not be copied, modified, or distributed
# except according to those terms.

# This is just a little script that can be downloaded from the internet to
# install $APP_NAME. It downloads the tarball from GitHub releases,
# and extracts it to ~/.cargo/bin/
#
# In the future this script will gain extra features, but for now it's
# intentionally very simplistic to avoid shipping broken things.

set -u

APP_NAME="{{APP_NAME}}"
APP_VERSION="{{APP_VERSION}}"
ARTIFACT_BASE_NAME="{{ARTIFACT_BASE_NAME}}"
ARTIFACT_DOWNLOAD_URL="{{ARTIFACT_DOWNLOAD_URL}}"

download_binary_and_run_installer() {
    downloader --check
    need_cmd mktemp
    need_cmd chmod
    need_cmd mkdir
    need_cmd rm
    need_cmd rmdir
    need_cmd tar
    need_cmd which
    need_cmd dirname
    need_cmd awk
    need_cmd cut


    get_architecture || return 1
    local _arch="$RETVAL"
    assert_nz "$_arch" "arch"

    local _ext=""
    local _zip_ext=".tar.xz"
    case "$_arch" in
        *windows*)
            _ext=".exe"
            _zip_ext=".zip"
            ;;
    esac

    local _tardir="$ARTIFACT_BASE_NAME-${_arch}"
    local _url="$ARTIFACT_DOWNLOAD_URL/${_tardir}${_zip_ext}"
    local _dir="$(mktemp -d 2>/dev/null || ensure mktemp -d -t $APP_NAME)"
    local _file="$_dir/input.tar.gz"

    # TODO: take a list of binaries each zip will contain so we know what to extract
    local _bin="$_dir/$APP_NAME$_ext"

    say "downloading $APP_NAME $APP_VERSION from $_url" 1>&2

    ensure mkdir -p "$_dir"
    downloader "$_url" "$_file"
    if [ $? != 0 ]; then
      say "failed to download $_url"
      say "this may be a standard network error, but it may also indicate"
      say "that $APP_NAME's release process is not working. When in doubt"
      say "please feel free to open an issue!"
      say "$REPO/issues/new/choose"
      exit 1
    fi

    ensure tar xf "$_file" --strip-components 1 -C "$_dir"

    install "$_bin" "$@"

    local _retval=$?

    ignore rm -rf "$_dir"

    return "$_retval"
}

install() {
    # FIXME: prefer $CARGO_HOME over $HOME/.cargo/ if available

    local _cargo_home_bin="$HOME/.cargo/bin/"
    local _bin="$1"
    mkdir -p "$_cargo_home_bin"
    cp "$_bin" "$_cargo_home_bin"

    say "installed $APP_NAME $APP_VERSION to $_cargo_home_bin"
}

get_architecture() {
    local _ostype="$(uname -s)"
    local _cputype="$(uname -m)"

    if [ "$_ostype" = Darwin -a "$_cputype" = i386 ]; then
        # Darwin `uname -s` lies
        if sysctl hw.optional.x86_64 | grep -q ': 1'; then
            local _cputype=x86_64
        fi
    fi

    if [ "$_ostype" = Darwin -a "$_cputype" = arm64 ]; then
        # Darwin `uname -s` doesn't seem to lie on Big Sur
        # but the cputype we want is called aarch64, not arm64 (they are equivalent)
        local _cputype=aarch64
    fi

    case "$_ostype" in
        Linux)
            if has_required_glibc; then
                local _ostype=unknown-linux-gnu
            else
                local _ostype=unknown-linux-musl
                say "Downloading musl binary."
            fi
            ;;

        Darwin)
            local _ostype=apple-darwin
            ;;

        MINGW* | MSYS* | CYGWIN*)
            local _ostype=pc-windows-msvc
            ;;

        *)
            err "no precompiled binaries available for OS: $_ostype"
            ;;
    esac

    case "$_cputype" in
        # these are the only two acceptable values for cputype
        x86_64 | aarch64 )
            ;;
        *)
            err "no precompiled binaries available for CPU architecture: $_cputype"

    esac

    local _arch="$_cputype-$_ostype"

    RETVAL="$_arch"
}

say() {
    local green=`tput setaf 2 2>/dev/null || echo ''`
    local reset=`tput sgr0 2>/dev/null || echo ''`
    echo "$1"
}

err() {
    local red=`tput setaf 1 2>/dev/null || echo ''`
    local reset=`tput sgr0 2>/dev/null || echo ''`
    say "${red}ERROR${reset}: $1" >&2
    exit 1
}

has_required_glibc() {
    local _ldd_version="$(ldd --version 2>&1 | head -n1)"
    # glibc version string is inconsistent across distributions
    # instead check if the string does not contain musl (case insensitive)
    if echo "${_ldd_version}" | grep -iv musl >/dev/null; then
        local _glibc_version=$(echo "${_ldd_version}" | awk 'NR==1 { print $NF }')
        local _glibc_major_version=$(echo "${_glibc_version}" | cut -d. -f1)
        local _glibc_min_version=$(echo "${_glibc_version}" | cut -d. -f2)
        local _min_major_version=2
        local _min_minor_version=17
        if [ "${_glibc_major_version}" -gt "${_min_major_version}" ] \
            || { [ "${_glibc_major_version}" -eq "${_min_major_version}" ] \
            && [ "${_glibc_min_version}" -ge "${_min_minor_version}" ]; }; then
            return 0
        else
            say "This operating system needs glibc >= ${_min_major_version}.${_min_minor_version}, but only has ${_libc_version} installed."
        fi
    else
        say "This operating system does not support dynamic linking to glibc."
    fi

    return 1
}

need_cmd() {
    if ! check_cmd "$1"
    then err "need '$1' (command not found)"
    fi
}

check_cmd() {
    command -v "$1" > /dev/null 2>&1
    return $?
}

need_ok() {
    if [ $? != 0 ]; then err "$1"; fi
}

assert_nz() {
    if [ -z "$1" ]; then err "assert_nz $2"; fi
}

# Run a command that should never fail. If the command fails execution
# will immediately terminate with an error showing the failing
# command.
ensure() {
    "$@"
    need_ok "command failed: $*"
}

# This is just for indicating that commands' results are being
# intentionally ignored. Usually, because it's being executed
# as part of error handling.
ignore() {
    "$@"
}

# This wraps curl or wget. Try curl first, if not installed,
# use wget instead.
downloader() {
    if check_cmd curl
    then _dld=curl
    elif check_cmd wget
    then _dld=wget
    else _dld='curl or wget' # to be used in error message of need_cmd
    fi

    if [ "$1" = --check ]
    then need_cmd "$_dld"
    elif [ "$_dld" = curl ]
    then curl -sSfL "$1" -o "$2"
    elif [ "$_dld" = wget ]
    then wget "$1" -O "$2"
    else err "Unknown downloader"   # should not reach here
    fi
}

download_binary_and_run_installer "$@" || exit 1