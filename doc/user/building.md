<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Building

## Pre-Build Requirements

### Python

Having Python 3 isn't an absolute requirement but it makes the subsequent steps to install the dependencies easier.

**Linux**

Use the system package manager to install Python 3 and the `venv` module (on Debian and Ubuntu the package is `python3-venv`). A system-wide `python3-pip` install is optional if you follow the recommended virtual environment below: the virtual environment gets its own `pip`, which avoids touching the distribution's managed environment (PEP 668).

> 💬 **Note:**
> The `-y` option allows `apt install` to run non-interactively.

```sh
sudo apt install -y python3 python3-venv
```

**Windows**

Download the Python 3 installer from [python.org](https://www.python.org/downloads/windows/) and run it manually, or use the following PowerShell commands.
Python 3.14.4 (64-bit Windows, latest stable release at the time) has been tested.

> 💬 **Note:**
> The `` ` `` is the PowerShell line continuation character.

```PowerShell
Invoke-WebRequest `
  https://www.python.org/ftp/python/3.14.4/python-3.14.4-amd64.exe `
  -OutFile python-3.14.4-amd64.exe

./python-3.14.4-amd64.exe /quiet PrependPath=1 Include_tcltk=0 Include_test=0
```

Confirm this Python is available on the `PATH`. Try closing and reopening the terminal if not.

```PowerShell
python --version
```

### Python Virtual Environment

For manual builds, a **virtual environment** in the repository root (for example, `.venv`) is recommended. It isolates the Python environment used for Conan and other `pip`-installed build tools from the system interpreter, pins compatible tool versions on `PATH`, and on Linux avoids PEP 668 "externally managed" errors when using the system Python.

**Linux**

```sh
python3 -m venv .venv
source .venv/bin/activate
pip install --upgrade pip
```

**Windows**

From PowerShell, use the following commands.

```PowerShell
python -m venv .venv
.\.venv\Scripts\Activate.ps1
# If PowerShell reports that "running scripts is disabled on this system"
# you can adjust the execution policy as follows and try again.
# Set-ExecutionPolicy -ExecutionPolicy RemoteSigned -Scope CurrentUser
python -m pip install --upgrade pip
```

Or use Command Prompt instead.

```bat
python -m venv .venv
.\.venv\Scripts\activate.bat
python -m pip install --upgrade pip
```

To exit the virtual environment later, run `deactivate` (same on Linux and Windows).

### CMake

The project requires CMake 3.17 or higher; dependencies may require a higher version.

There are x86_64 and arm64 packages for CMake 3.30.0 on the [Python Package Index (PyPI)](https://pypi.org/) which have been tested.

**Linux**

```sh
pip install cmake~=3.17
```

**Windows**

```PowerShell
pip install cmake~=3.17
```

### Conan

Using [Conan](https://conan.io/) simplifies fetching, building, and installing the required C++ dependencies from [Conan Center](https://conan.io/center/).

The project requires Conan 2.2 or higher; dependencies may require a higher version.
Conan 2.28.0 (latest release at the time) has been tested.

**Linux**

```sh
pip install conan~=2.2 --upgrade
conan profile detect
```

The detected profile is displayed, along with some `WARN` messages.
For example, the following profile has been tested.

```ini
[settings]
arch=x86_64
build_type=Release
compiler=gcc
compiler.cppstd=gnu17
compiler.libcxx=libstdc++11
compiler.version=13
os=Linux
```

**Windows**

```PowerShell
pip install conan~=2.2 --upgrade
conan profile detect
notepad (conan profile path default)
```

You may wish to edit the detected profile.
Conan Center prebuilt binaries target MSVC 193/194, not 195 (Visual Studio 2026).
Pin MSVC 194 so `conan install` downloads packages instead of building from source, with `compiler.version=194` and `compiler.cppstd=17`.

The following settings have been tested.

```ini
[settings]
arch=x86_64
build_type=Release
compiler=msvc
compiler.cppstd=17
compiler.runtime=dynamic
compiler.version=194
os=Windows
```

## Building the NvNmos Library

**Linux**

Prepare a `build` directory adjacent to the `src` directory.

```sh
mkdir build
```

To install the dependencies using Conan, use the following command.

> 💬 **Note:**
> Replace `<Release-or-Debug>` with the necessary value.
> Passing `--lockfile=src/conan.lock` pins dependency versions per `src/conan.lock` (Conan Center `nmos-cpp` package graph).
> Do not pass `-g CMakeToolchain` or `-g CMakeDeps`; `src/conanfile.py` already generates them, and Conan fails if they are duplicated (for example when copying older command lines).

```sh
conan install src \
  --settings:all build_type=<Release-or-Debug> \
  --build=missing \
  --output-folder=src/conan \
  --lockfile=src/conan.lock
```

Use the following CMake command to configure the build.

> 💬 **Note:**
> Replace `<Release-or-Debug>` with the necessary value.
> The `CMAKE_TOOLCHAIN_FILE` path is resolved relative to the top-level `src` directory passed as the last argument.

```sh
cmake -B build \
  -DCMAKE_TOOLCHAIN_FILE=conan/conan_toolchain.cmake \
  -DCMAKE_BUILD_TYPE=<Release-or-Debug> \
  src
```

Build the library and example application.

```sh
cmake --build build --parallel
```

**Windows**

Prepare a `build` directory adjacent to the `src` directory.

```PowerShell
mkdir build
```

To install the dependencies using Conan, use the following command.

> 💬 **Note:**
> The `` ` `` is the PowerShell line continuation character. In the Windows command prompt, use `^` instead.
> Replace `<Release-or-Debug>` with the necessary value.
> Passing `--lockfile=src/conan.lock` pins dependency versions per `src/conan.lock` (Conan Center `nmos-cpp` package graph).
> Do not pass `-g CMakeToolchain` or `-g CMakeDeps`; `src/conanfile.py` already generates them, and Conan fails if they are duplicated (for example when copying older command lines).

```PowerShell
conan install src `
  --settings:all build_type=<Release-or-Debug> `
  --build=missing `
  --output-folder=src/conan `
  --lockfile=src/conan.lock
```

Repeat the command for both `Debug` and `Release` if required.

Use the following CMake command to configure the build.

```PowerShell
cmake -B build `
  -G "Visual Studio 17 2022" `
  -DCMAKE_TOOLCHAIN_FILE="conan/conan_toolchain.cmake" `
  -DCMAKE_CONFIGURATION_TYPES="Debug;Release" `
  src
```

Build the library and application with the following command or manually using the generated Visual Studio solution.

> 💬 **Note:**
> Replace `<Release-or-Debug>` with the necessary value.

```sh
cmake --build build --config <Release-or-Debug> --parallel
```

### Local nmos-cpp Checkout

To build against a clone of [nmos-cpp](https://github.com/sony/nmos-cpp) while using Conan to resolve Boost, cpprestsdk, Avahi or mDNSResponder, and other dependencies:

1. Clone it into a directory `nmos-cpp` in the same parent directory as `nvnmos`.

2. Install Conan dependencies with the consumer option `nmos_cpp_from_source=True` and **without** the default lockfile, which is for the packaged `nmos-cpp` graph:

   **Linux**

   ```sh
   conan install src \
     --settings:all build_type=<Release-or-Debug> \
     --build=missing \
     --output-folder=src/conan \
     --lockfile="" \
     -o "&:nmos_cpp_from_source=True"
   ```

   **Windows**

   ```PowerShell
   conan install src `
     --settings:all build_type=<Release-or-Debug> `
     --build=missing `
     --output-folder=src/conan `
     --lockfile="" `
     -o "&:nmos_cpp_from_source=True"
   ```

   > 💬 **Note:**
   > With `--lockfile=""`, versions of dependencies such as Boost and OpenSSL are not pinned. You may choose to create a lockfile for `nmos_cpp_from_source=True` installs (`conan lock create` per profile, then `conan lock merge`), then pass `--lockfile=<path>` on `conan install`.

3. Configure and build as in **Building the NvNmos Library** above, adding `-DUSE_ADD_SUBDIRECTORY=ON` to the `cmake` configure step. (If you followed step 1, you do not need `-DNMOS_CPP_DIRECTORY=<path-to-nmos-cpp/Development>`.)

## Run-Time Requirements

**Linux**

Install and run the Avahi Daemon.

```sh
apt update
apt install -y dbus avahi-daemon

/etc/init.d/dbus start
/etc/init.d/avahi-daemon start
```

> 💬 **Note:**
> Since Ubuntu 24.04, an init script is not provided for the Avahi daemon; run `avahi-daemon --daemonize` instead.

**Windows**

Install and start the Bonjour Service.

See [Download Bonjour Print Services for Windows v2.0.2](https://support.apple.com/kb/DL999).
