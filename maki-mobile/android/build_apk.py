#!/usr/bin/env python3
"""Build the phone app's APK without Gradle.

The app has no Java of its own: it is one NativeActivity and a shared library.
So the APK is four steps, all tools from the SDK build-tools: link the manifest
with aapt2, add the libraries, align, sign.

    python maki-mobile/android/build_apk.py            # release, arm64 + x86_64
    python maki-mobile/android/build_apk.py --debug    # debug, one ABI
    python maki-mobile/android/build_apk.py --install  # then adb install + start
"""

import argparse
import glob
import os
import shutil
import subprocess
import sys
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent
ANDROID = Path(__file__).resolve().parent
PACKAGE = "dev.makima.mobile"
LIBRARY = "makima_mobile"
MIN_SDK = 26
TARGET_SDK = 35
COMPILE_SDK = 35
ABIS = {
    "arm64-v8a": "aarch64-linux-android",
    "x86_64": "x86_64-linux-android",
}
KEYSTORE_ALIAS = "androiddebugkey"
KEYSTORE_PASS = "android"


def sdk_root() -> Path:
    for name in ("ANDROID_HOME", "AndroidSdkDir", "ANDROID_SDK_ROOT"):
        value = os.environ.get(name)
        if value and Path(value).is_dir():
            return Path(value)
    raise SystemExit("no Android SDK: set ANDROID_HOME")


def newest(directory: Path) -> Path:
    found = [Path(p) for p in glob.glob(str(directory / "*")) if Path(p).is_dir()]
    if not found:
        raise SystemExit(f"nothing installed under {directory}")
    return max(found, key=lambda p: [int(part) if part.isdigit() else 0 for part in p.name.split(".")])


def sdk_tool(sdk: Path, tool: str) -> Path:
    build_tools = os.environ.get("ANDROID_BUILD_TOOLS")
    directory = sdk / "build-tools" / build_tools if build_tools else newest(sdk / "build-tools")
    for suffix in (".exe", ".bat"):
        candidate = directory / (tool + suffix)
        if candidate.exists():
            return candidate
    raise SystemExit(f"{tool} is not in {directory}")


def ndk_root(sdk: Path) -> Path:
    version = os.environ.get("ANDROID_NDK_VERSION")
    return sdk / "ndk" / version if version else newest(sdk / "ndk")


def java() -> Path:
    home = os.environ.get("JAVA_HOME")
    candidates = [Path(home) / "bin" / "java.exe"] if home else []
    candidates += [Path(p) / "bin" / "java.exe" for p in glob.glob(r"C:\Program Files\Android\openjdk\jdk-*")]
    for candidate in candidates:
        if candidate.exists():
            return candidate
    found = shutil.which("java")
    if found:
        return Path(found)
    raise SystemExit("apksigner needs a JDK: set JAVA_HOME")


def run(command, env=None, cwd=None):
    print("+", " ".join(str(part) for part in command), flush=True)
    subprocess.run([str(part) for part in command], check=True, env=env, cwd=cwd)


def build_library(target: str, ndk: Path, debug: bool) -> Path:
    toolchain = ndk / "toolchains" / "llvm" / "prebuilt" / "windows-x86_64" / "bin"
    linker = toolchain / f"{target}26-clang.cmd"
    if not linker.exists():
        raise SystemExit(f"no NDK linker for {target}: {linker}")
    profile = "debug" if debug else "release"
    env = dict(os.environ)
    env[f"CARGO_TARGET_{target.upper().replace('-', '_')}_LINKER"] = str(linker)
    run(
        ["cargo", "build", "-p", "maki-mobile", "--target", target]
        + ([] if debug else ["--release"]),
        env=env,
        cwd=ROOT,
    )
    library = ROOT / "target" / target / profile / f"lib{LIBRARY}.so"
    if not library.exists():
        raise SystemExit(f"cargo produced no {library}")
    return library


def assemble(sdk: Path, abis: dict[str, Path], debug: bool) -> Path:
    out = ROOT / "target" / "android"
    out.mkdir(parents=True, exist_ok=True)
    unsigned = out / "makima-unsigned.apk"
    aapt2 = sdk_tool(sdk, "aapt2")
    platform = sdk / "platforms" / f"android-{COMPILE_SDK}" / "android.jar"
    run(
        [
            aapt2,
            "link",
            "-o",
            unsigned,
            "--manifest",
            ANDROID / "AndroidManifest.xml",
            "-I",
            platform,
            "--min-sdk-version",
            MIN_SDK,
            "--target-sdk-version",
            TARGET_SDK,
        ]
    )
    with zipfile.ZipFile(unsigned, "a") as apk:
        for abi, library in abis.items():
            apk.write(library, f"lib/{abi}/{library.name}")
    return unsigned


def sign(sdk: Path, unsigned: Path, debug: bool) -> Path:
    keystore = Path.home() / ".android" / "debug.keystore"
    keystore.parent.mkdir(parents=True, exist_ok=True)
    if not keystore.exists():
        run(
            [
                java().parent / "keytool.exe",
                "-genkeypair",
                "-keystore",
                keystore,
                "-storepass",
                KEYSTORE_PASS,
                "-keypass",
                KEYSTORE_PASS,
                "-alias",
                KEYSTORE_ALIAS,
                "-keyalg",
                "RSA",
                "-keysize",
                "2048",
                "-validity",
                "10000",
                "-dname",
                "CN=Android Debug,O=Android,C=US",
            ]
        )
    aligned = unsigned.with_name(unsigned.stem.replace("-unsigned", "") + "-aligned.apk")
    run([sdk_tool(sdk, "zipalign"), "-f", "-p", "4", unsigned, aligned])
    signed = unsigned.with_name(unsigned.stem.replace("-unsigned", "") + ("-debug" if debug else "") + ".apk")
    env = dict(os.environ)
    env["JAVA_HOME"] = str(java().parent.parent)
    env["PATH"] = str(java().parent) + os.pathsep + env.get("PATH", "")
    run(
        [
            sdk_tool(sdk, "apksigner"),
            "sign",
            "--ks",
            keystore,
            "--ks-pass",
            f"pass:{KEYSTORE_PASS}",
            "--key-pass",
            f"pass:{KEYSTORE_PASS}",
            "--out",
            signed,
            aligned,
        ],
        env=env,
    )
    aligned.unlink()
    unsigned.unlink()
    return signed


def install(apk: Path):
    adb = shutil.which("adb") or str(sdk_root() / "platform-tools" / "adb.exe")
    run([adb, "install", "-r", apk])
    run([adb, "shell", "am", "start", "-n", f"{PACKAGE}/android.app.NativeActivity"])


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--debug", action="store_true", help="debug profile and one ABI")
    parser.add_argument("--abi", choices=sorted(ABIS), action="append", help="ship only these ABIs")
    parser.add_argument("--install", action="store_true", help="install and start on the attached device")
    args = parser.parse_args()

    sdk = sdk_root()
    ndk = ndk_root(sdk)
    abis = args.abi or (["arm64-v8a"] if args.debug else list(ABIS))
    libraries = {
        abi: build_library(ABIS[abi], ndk, args.debug)
        for abi in abis
    }
    unsigned = assemble(sdk, libraries, args.debug)
    apk = sign(sdk, unsigned, args.debug)
    print(f"apk: {apk}")
    if args.install:
        install(apk)
    return 0


if __name__ == "__main__":
    sys.exit(main())
