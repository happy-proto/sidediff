#!/usr/bin/env -S uv run --script
#
# /// script
# requires-python = ">=3.15"
# dependencies = [
#     "typer>=0.27.3",
# ]
# ///


"""生成发布包内二进制体积的折叠 Markdown 表格。"""

import tarfile
import zipfile
from pathlib import Path
from typing import Annotated

import typer

PLATFORMS = {
    "unknown-linux-gnu": "Linux（GNU）",
    "apple-darwin": "macOS",
    "pc-windows-msvc": "Windows",
}


def binary_size(archive: Path) -> tuple[str, int]:
    """读取归档中可执行文件的未压缩大小。"""
    if archive.name.endswith(".tar.xz"):
        package = archive.name.removesuffix(".tar.xz")
        with tarfile.open(archive, "r:xz") as bundle:
            binary = bundle.getmember(f"{package}/sidediff")
            if not binary.isfile():
                raise ValueError(f"{archive.name} 中的 sidediff 不是普通文件")
            size = binary.size
    else:
        package = archive.name.removesuffix(".zip")
        with zipfile.ZipFile(archive) as bundle:
            binary = bundle.getinfo(f"{package}/sidediff.exe")
            if binary.is_dir():
                raise ValueError(f"{archive.name} 中的 sidediff.exe 是目录")
            size = binary.file_size
    return package.removeprefix("sidediff-"), size


def main(
    asset_dir: Annotated[
        Path,
        typer.Argument(
            exists=True,
            file_okay=False,
            readable=True,
            resolve_path=True,
            help="包含 sidediff-*.tar.xz 与 sidediff-*.zip 发布包的目录。",
        ),
    ],
) -> None:
    """将二进制体积表格输出到标准输出，单位为 MiB，保留两位小数。"""
    archives = sorted(asset_dir.glob("sidediff-*.tar.xz")) + sorted(
        asset_dir.glob("sidediff-*.zip")
    )
    if not archives:
        typer.echo("没有找到发布包。", err=True)
        raise typer.Exit(1)

    rows = []
    try:
        for archive in archives:
            target, size = binary_size(archive)
            arch, _, platform = target.partition("-")
            if platform not in PLATFORMS:
                raise ValueError(f"无法识别发布平台：{target}")
            arch = "ARM64" if arch == "aarch64" else arch
            rows.append(
                f"| {PLATFORMS[platform]} | {arch} | {size / 1048576:.2f} MiB |"
            )
    except (OSError, tarfile.TarError, zipfile.BadZipFile, KeyError, ValueError) as exc:
        typer.echo(f"无法统计二进制体积：{exc}", err=True)
        raise typer.Exit(1) from exc

    typer.echo(
        "<details>\n<summary>二进制体积</summary>\n\n"
        "按安装到目标设备后的可执行文件大小统计。\n\n"
        "| 平台 | 架构 | 体积 |\n| --- | --- | ---: |\n"
        + "\n".join(rows)
        + "\n\n</details>"
    )


if __name__ == "__main__":
    typer.run(main)
