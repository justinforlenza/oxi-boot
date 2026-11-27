# Third-party Licences

The server sends these files to clients. They are separate programs: oxi-boot does not link to them, change them, or contain their code.

| File | Program | Licence | Version in the container image | Source |
| --- | --- | --- | --- | --- |
| `undionly.kpxe`, `snponly.efi`, `snponly-arm64.efi` | iPXE | GPL-2.0-or-later (parts also under the UBDL) | v2.0.0, from `ipxeboot.tar.gz` | https://github.com/ipxe/ipxe/tree/v2.0.0 |
| `wimboot` | wimboot | GPL-2.0-or-later | v2.9.0 | https://github.com/ipxe/wimboot/tree/v2.9.0 |

`snponly-arm64.efi` is iPXE's `arm64/snponly.efi` under another name; its content is unchanged.

The release workflow downloads both from the projects' own release pages, checks each against a fixed checksum, and writes the versions and source links to `boot/SOURCES.txt`, which ships in the download and in the image.
