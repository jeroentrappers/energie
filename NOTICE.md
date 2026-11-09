# Third-party notices

energie is licensed under the [GNU AGPL v3.0 or later](LICENSE). Copyright © 2026 Jeroen
Trappers.

## Work this project builds on

Protocol details, API constants and data tables in this project were derived from the projects
below, which is gratefully acknowledged. Their licenses permit this use and require these
notices to be kept.

| Project | Copyright holder | License | Used for |
| --- | --- | --- | --- |
| [hyundai_kia_connect_api](https://github.com/Hyundai-Kia-Connect/hyundai_kia_connect_api) and [kia_uvo](https://github.com/Hyundai-Kia-Connect/kia_uvo) | Their contributors | MIT | Kia Connect protocol and constants |
| [HA_VAG-EU-Data-Act](https://github.com/TommiG1/HA_VAG-EU-Data-Act) | TommiG1 and contributors | MIT | VW Group portal client and `crates/cloud/data/euda_fields.json`, reduced from its data dictionary |
| [Fluvius-home-assistant](https://github.com/sander110419/Fluvius-home-assistant) | sander110419 and contributors | MIT | Mijn Fluvius login and data |
| [tuya-local](https://github.com/make-all/tuya-local) | make-all and contributors | MIT | Tuya cable data points |
| [tinytuya](https://github.com/jasonacox/tinytuya) | Jason A. Cox and contributors | MIT | Tuya 3.4 local protocol |
| [home-assistant-omnik-inverter](https://github.com/robbinjanssen/home-assistant-omnik-inverter) | Robbin Janssen and contributors | MIT | Omnik status format |
| [solis_modbus](https://github.com/Pho3niX90/solis_modbus) | Pho3niX90 and contributors | MIT | Solis register map and write sequences |
| [SmartEVSE](https://github.com/SmartEVSE/SmartEVSE-3) and [SmartEVSE-3.5](https://github.com/dingo35/SmartEVSE-3.5) | Their contributors | MIT | SmartEVSE MQTT topics |
| [pythinqconnect](https://github.com/thinq-connect/pythinqconnect) | LG Electronics | Apache-2.0 | LG ThinQ Connect API |
| [hass-entso-e](https://github.com/JaccoR/hass-entso-e) | JaccoR and contributors | Apache-2.0 | Price statistics |
| [astral](https://github.com/sffjunkie/astral) | Simon Kennedy | Apache-2.0 | Reference values for the solar position |

### MIT license text

> Permission is hereby granted, free of charge, to any person obtaining a copy of this software
> and associated documentation files (the "Software"), to deal in the Software without
> restriction, including without limitation the rights to use, copy, modify, merge, publish,
> distribute, sublicense, and/or sell copies of the Software, and to permit persons to whom the
> Software is furnished to do so, subject to the following conditions:
>
> The above copyright notice and this permission notice shall be included in all copies or
> substantial portions of the Software.
>
> THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING
> BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND
> NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM,
> DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
> OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

The Apache-2.0 license text is at <https://www.apache.org/licenses/LICENSE-2.0>.

## Rust dependencies

The binaries link Rust crates under permissive licenses, including MIT, Apache-2.0, BSD, ISC and
Zlib. The full list and versions are in `Cargo.lock`, and `deny.toml` lists the licenses CI
accepts. To list each crate with its license, run `cargo metadata --format-version 1`.
