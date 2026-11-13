"""Fetch a Tuya device's local key through Home Assistant's Tuya (cloud) integration.

Runs inside the Home Assistant container, which has the `tuya_sharing` SDK and HA's signed-in
Tuya session. It asks Tuya's cloud for one device and writes its local key to stdout, so it can
be piped straight into a credentials file; the device's details and its local DP map go to
stderr. See docs/HARDWARE.md, "Finding the local key".

    docker exec -i <ha-container> python3 - <device-id> < scripts/tuya-local-key.py

It borrows HA's access token and never refreshes it: a refresh would hand HA a token it no
longer knows about and sign the integration out. With less than 15 minutes left on the token it
stops; run it again after HA has refreshed (within the hour).
"""

import json
import sys
import time

from homeassistant.components.tuya.const import TUYA_CLIENT_ID
from tuya_sharing.customerapi import CustomerApi, CustomerTokenInfo
from tuya_sharing.device import DeviceRepository

CONFIG_ENTRIES = "/config/.storage/core.config_entries"
MIN_TOKEN_MINUTES = 15


def err(*a):
    print(*a, file=sys.stderr)


class NoRefresh:
    """Token listener that refuses: the SDK must not replace HA's token."""

    def update_token(self, _token):
        sys.exit("the SDK tried to refresh HA's Tuya token; aborted")


def main():
    if len(sys.argv) != 2:
        sys.exit("usage: python3 - <device-id> < tuya-local-key.py")
    device_id = sys.argv[1]
    entries = json.load(open(CONFIG_ENTRIES))["data"]["entries"]
    tuya = [e["data"] for e in entries if e["domain"] == "tuya"]
    if not tuya:
        sys.exit("no Tuya integration configured in Home Assistant")
    entry = tuya[0]
    token = entry["token_info"]
    left = (token["t"] + token["expire_time"] * 1000 - time.time() * 1000) / 60000
    if left < MIN_TOKEN_MINUTES:
        sys.exit(f"HA's Tuya token expires in {left:.0f} min; try again after HA refreshes it")

    api = CustomerApi(CustomerTokenInfo(token), TUYA_CLIENT_ID, entry["user_code"], entry["endpoint"], NoRefresh())
    api.refresh_access_token_if_need = lambda: None
    found = DeviceRepository(api).query_devices_by_ids([device_id])
    if not found:
        sys.exit(f"device {device_id} not found in this Tuya account")
    d = found[0]
    if not d.local_key:
        sys.exit(f"Tuya returned no local key for {device_id}")

    err(f"name {d.name!r}, product {d.product_name!r} ({d.product_id}), category {d.category}")
    err(f"online {d.online}, support_local {d.support_local}, ip (as Tuya sees it) {d.ip}")
    err("local DPs (RW = settable):")
    for dp, v in sorted(d.local_strategy.items(), key=lambda x: int(x[0])):
        code = v["status_code"]
        item = v["config_item"]
        err(f"  {dp:>3} {code:<24} {'RW' if code in d.function else 'RO'} {item['valueType']} {item['valueDesc']}")
    print(d.local_key)


if __name__ == "__main__":
    main()
