"""Regenerate the Display P3 verifier fixtures beside this script.

The profile is a real Display P3 ICC profile for test purposes:
Pillow's valid sRGB ICC skeleton supplies the D65 white point and
the sRGB transfer curves (both are also Display P3's), while the
description, chromaticity tags, and RGB colourant matrix are
replaced with Display P3's primaries. That is enough for PNG to
carry a genuine Display P3 iCCP chunk and for Pillow to read the
same profile bytes back.

The images are deliberately tiny and exact:
- layer: six fully opaque product pixels, plus transparent and
  semi-transparent pixels around them;
- output: identical at every fully opaque layer pixel and
  deliberately different everywhere else;
- output-tampered: the output with exactly one fully opaque
  product pixel changed by one red least-significant bit.
"""

from __future__ import annotations

import struct
from io import BytesIO
from pathlib import Path

from PIL import Image, ImageCms

HERE = Path(__file__).resolve().parent
SIZE = (6, 4)


def _fixed16(value: float) -> bytes:
    return struct.pack(">i", round(value * 65536))


def display_p3_profile() -> bytes:
    profile = bytearray(
        ImageCms.ImageCmsProfile(ImageCms.createProfile("sRGB")).tobytes()
    )
    tag_count = struct.unpack(">I", profile[128:132])[0]
    tags = {}
    for index in range(tag_count):
        offset = 132 + index * 12
        signature = bytes(profile[offset : offset + 4])
        data_offset, data_size = struct.unpack(
            ">II", profile[offset + 4 : offset + 12]
        )
        tags[signature] = (data_offset, data_size)

    # mluc description. Keep the byte length unchanged by padding
    # the replacement text with spaces; parsers trim it in display.
    offset, _ = tags[b"desc"]
    text_length, text_offset = struct.unpack(
        ">II", profile[offset + 20 : offset + 28]
    )
    description = "Display P3   ".encode("utf-16-be")
    assert len(description) == text_length
    start = offset + text_offset
    profile[start : start + text_length] = description

    # RGB -> XYZ (D65) matrix columns derived from the Display P3
    # primaries and D65 white point.
    matrix = {
        b"rXYZ": (0.486570949, 0.228974564, 0.0),
        b"gXYZ": (0.26566769, 0.69173852, 0.04511338),
        b"bXYZ": (0.19821729, 0.07928691, 1.04394437),
    }
    for signature, xyz in matrix.items():
        offset, _ = tags[signature]
        assert profile[offset : offset + 4] == b"XYZ "
        profile[offset + 8 : offset + 20] = b"".join(
            _fixed16(value) for value in xyz
        )

    offset, _ = tags[b"chrm"]
    primaries = [(0.680, 0.320), (0.265, 0.690), (0.150, 0.060)]
    for index, (x, y) in enumerate(primaries):
        start = offset + 12 + index * 8
        profile[start : start + 8] = _fixed16(x) + _fixed16(y)

    result = bytes(profile)
    parsed = ImageCms.ImageCmsProfile(BytesIO(result))
    assert parsed.profile.profile_description.strip() == "Display P3"
    return result


def _pixels():
    layer_pixels = []
    width, height = SIZE
    for y in range(height):
        for x in range(width):
            if x in (1, 2, 4) and y in (1, 2):
                layer_pixels.append(
                    (
                        (37 + x * 41) % 256,
                        (91 + y * 53) % 256,
                        (149 + x * 17 + y * 29) % 256,
                        255,
                    )
                )
            elif (x + y) % 3 == 0:
                layer_pixels.append((220, 30, 90, 0))
            else:
                layer_pixels.append((15, 180, 75, 128))

    output_pixels = []
    for index, pixel in enumerate(layer_pixels):
        if pixel[3] == 255:
            output_pixels.append(pixel)
        else:
            output_pixels.append(
                (
                    (index * 31) % 256,
                    (index * 47) % 256,
                    (index * 71) % 256,
                    (index * 37) % 256,
                )
            )

    tampered_pixels = list(output_pixels)
    for index, pixel in enumerate(layer_pixels):
        if pixel[3] == 255:
            old = tampered_pixels[index]
            tampered_pixels[index] = (old[0] ^ 1, old[1], old[2], old[3])
            break

    return layer_pixels, output_pixels, tampered_pixels


def main() -> None:
    profile = display_p3_profile()
    names_and_pixels = zip(
        ("layer", "output", "output-tampered"),
        _pixels(),
        strict=True,
    )
    for name, pixels in names_and_pixels:
        image = Image.new("RGBA", SIZE)
        image.putdata(pixels)
        path = HERE / f"display-p3-{name}-v1.png"
        image.save(path, format="PNG", icc_profile=profile)
        with Image.open(path) as loaded:
            loaded.load()
            assert loaded.info.get("icc_profile") == profile
        print(path.name)


if __name__ == "__main__":
    main()
