# Reference material

The specs `modbus` is checked against. Cited, not held: both are © Modbus Organization, served
behind its terms, so no copy enters this repository. A maintainer's local copy — a Markdown
conversion, with the figures — sits beside this file and is git-ignored.

| Document                                               | Version, date      | Subject                                      |
| ------------------------------------------------------ | ------------------ | -------------------------------------------- |
| [MODBUS Application Protocol Specification][app]       | V1.1b3, 2012-04-26 | The PDU: function codes, reads, exceptions   |
| [MODBUS Messaging on TCP/IP Implementation Guide][tcp] | V1.0b, 2006-10-24  | The MBAP header, connections, reply handling |

Both listed at <https://www.modbus.org/modbus-specifications>; fetched 2026-10-07. Local copies are
Mistral OCR conversions of those PDFs (`modbusprotocolspecification.pdf/`,
`messagingimplementationguide.pdf/`): machine transcription, so a table or a figure caption may
carry an OCR error the PDF does not. Where they disagree, the PDF wins.

The versions above are what the crate was checked against. A later revision under the same URL is a
reason to re-check, not an update to take on trust.

[app]: https://www.modbus.org/file/secure/modbusprotocolspecification.pdf
[tcp]: https://www.modbus.org/file/secure/messagingimplementationguide.pdf
