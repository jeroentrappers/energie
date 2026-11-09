// The dashboard: live tiles and the power flow from the SSE stream, charts from /api/history
// and /api/prices. Views, labels and colours follow the HA dashboard "energie-beheer".
"use strict";
const T = I18N.T, LOCALE = I18N.locale;
const $ = (id) => document.getElementById(id);
const nil = (v) => v === null || v === undefined || Number.isNaN(v);
const num = (v, d = 0) => {
  if (nil(v)) return "–";
  const x = Math.round(Number(v) * 10 ** d) === 0 ? 0 : Number(v); // no "-0"
  return x.toLocaleString(LOCALE, { minimumFractionDigits: d, maximumFractionDigits: d });
};
const W = (v) => nil(v) ? "–" : (Math.abs(v) >= 1000 ? num(v / 1000, 2) + " kW" : num(v) + " W");
const Wf = (v) => nil(v) ? "–" : num(v) + " W";
const kWh = (v, d = 2) => nil(v) ? "–" : num(v, d) + " kWh";
const eur = (v, d = 2) => nil(v) ? "–" : "€ " + num(v, d);
const eurkwh = (v) => nil(v) ? "–" : "€ " + num(v, 3) + "/kWh";
const pct = (v, d = 0) => nil(v) ? "–" : num(v, d) + " %";
const amp = (v, d = 1) => nil(v) ? "–" : num(v, d) + " A";
const km = (v) => nil(v) ? "–" : num(v, 1) + " km";
const onoff = (b) => nil(b) ? "–" : (b ? "aan" : "uit");
const esc = (s) => String(s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));
const hhmm = (t) => nil(t) ? "–" : new Date(t).toLocaleTimeString(LOCALE, { hour: "2-digit", minute: "2-digit" });
const time = (iso) => iso ? new Date(iso).toLocaleTimeString(LOCALE, { hour: "2-digit", minute: "2-digit", second: "2-digit" }) : "";
const val = (v) => nil(v) ? "–" : (typeof v === "number" ? num(v, Math.abs(v) < 10 ? 3 : 0) : esc(v));
const RC = { None: "zelfverbruik", ForceCharge: "geforceerd laden", ForceDischarge: "geforceerd ontladen" };
const HOUR = 3600000, DAY = 86400000;
const startOfDay = (t, add = 0) => { const d = new Date(t); d.setHours(0, 0, 0, 0); d.setDate(d.getDate() + add); return d.getTime(); };
const startOfHour = (t) => { const d = new Date(t); d.setMinutes(0, 0, 0); return d.getTime(); };
const quarterStart = (t) => t - (t % 900000);
const store = {
  get: (k, d) => { try { return localStorage.getItem(k) ?? d; } catch { return d; } },
  set: (k, v) => { try { localStorage.setItem(k, v); } catch { /* private mode */ } },
};

const S = { snap: null, now: Date.now(), prices: null, hist: null, tab: "overzicht", cache: {}, fetched: {} };

// The entity registry (`/api/entities`), kept current from the `entities` changes in the event
// stream. A cloud device's values beyond the World (the EV3's DC limit, the Fluvius monthly
// peaks) are found through the device that fills a World section: `driver:<id>` in the sources.
let entities = [];
const entity = (id) => entities.find((e) => e.id === id);
const devOf = (s, section) => { const src = (s.sources || {})[section] || ""; return /^driver:[a-z0-9_]+$/.test(src) ? src.slice(7) : null; };
const devVal = (dev, key) => { const e = dev && entity(dev + "_" + key); return e ? e.value : null; };
const when = (iso) => nil(iso) ? "–" : new Date(iso).toLocaleString(LOCALE, { day: "numeric", month: "short", hour: "2-digit", minute: "2-digit" });

// ------------------------------------------------------------------ tiles
function tile(k, v, s = "", cls = "") {
  return `<div class="tile ${cls}"><div class="k">${esc(T(k))}</div><div class="v">${T(v)}</div><div class="s">${T(s)}</div></div>`;
}
const fill = (id, html) => { const e = $(id); if (e) e.innerHTML = html; };
const pill = (k, v, cls = "") => `<span class="pill ${cls}">${esc(T(k))} <b>${T(v)}</b></span>`;

// A device slot nothing reads stays out of the page.
const hasDevice = (s, section) => !["unavailable", "none", undefined].includes((s.sources || {})[section]);
function kmSince(odo, start) { return nil(odo) ? null : odo - start; }

function renderTiles(s) {
  const w = s.world, d = s.derived, m = s.memory, st = s.settings;
  const p1 = w.p1.power_w, batW = w.solis.battery_power_w;
  const risk = d.capaciteitspiek_risico;
  const nb = !!w.no_battery, ev3Here = hasDevice(s, "cable"), buzzHere = hasDevice(s, "evse");
  document.body.classList.toggle("no-battery", nb);
  document.body.classList.toggle("no-ev3", !ev3Here);
  document.body.classList.toggle("no-buzz", !buzzHere);
  const next = m.steering_volgende_update ? hhmm(Date.parse(m.steering_volgende_update)) : "–";

  fill("b_overzicht", [
    pill("Prijs nu", eurkwh(d.stroomprijs_nu)), pill("Niveau", esc(d.stroomprijs_niveau || "–")),
    nb ? "" : pill("Batterij", pct(w.solis.soc)), nb ? "" : pill("Volgende sturing", next), pill("Kwartier", W(d.kwartiervermogen_projectie_w)),
    pill("Maandpiek", W(w.p1.month_peak_w)), risk ? pill("PIEKRISICO", "aan", "risk") : "",
  ].join(""));
  fill("b_capaciteit", [pill("Projectie", W(d.kwartiervermogen_projectie_w)), pill("Vastgelegd", num(d.kwartiervermogen_vastgelegd_kw, 3) + " kW"), pill("Net nu", W(p1))].join(""));
  fill("b_batterij", [pill("SOC", pct(w.solis.soc)), pill("Beschikbaar", kWh(d.batterij_beschikbare_energie_kwh)), pill("Modus", RC[w.solis.rc_mode] || "–")].join(""));
  fill("b_ev", [ev3Here ? pill("EV3", pct(w.ev3.soc)) : "", buzzHere ? pill("ID.Buzz", pct(w.buzz.soc)) : "", pill("Prijs", esc(d.stroomprijs_niveau || "–"))].join(""));

  const pv = d.pv_vermogen_totaal_w;
  fill("t_stromen", [
    tile("Net (P1)", `<span class="${p1 > 0 ? "pos" : "neg"}">${W(p1)}</span>`, nil(p1) ? "" : (p1 > 0 ? "afname" : "injectie")),
    tile("Zon", W(pv), "Solis " + W(w.solis.pv_power_w) + " · Omnik " + W(w.omnik.power_w)),
    tile("Huis", W(d.huisverbruik_vermogen_w), T("basis") + " " + W(d.huisverbruik_excl_stuurbaar_w)),
    nb ? "" : tile("Batterij", W(batW), nil(batW) ? "" : T(batW > 0 ? "ontlaadt" : batW < 0 ? "laadt" : "rust") + " · " + pct(w.solis.soc)),
    !ev3Here ? "" : tile("Kia EV3 (laadkabel)", W(d.ev_laadkabel_vermogen_w), esc(w.cable.status || "–")),
    !buzzHere ? "" : tile("VW ID.Buzz (SmartEVSE)", W(d.ev_laadvermogen_smartevse_w), esc(w.evse.state || "–")),
  ].join(""));

  const sp = d.batterij_setpoint_w;
  const doel = sp < 0 ? T("laden") + " " + num(-sp) + " W" : sp > 0 ? T("ontladen") + " " + num(sp) + " W" : T("vasthouden");
  const ev3 = ["charging", "waiting"].includes(w.cable.status) ? `${T("doel")} ${num((w.cable.set_current_a || 0) * 230)} W (${num(w.cable.set_current_a || 0, 1)} A)` : "—";
  const buzz = w.evse.state === "Charging" ? `${T("doel")} ${num((w.evse.override_a || 0) * 230)} W (${num(w.evse.override_a || 0, 1)} A)` : "—";
  fill("laaddoelen", [nb ? "" : `<li>🔋 <b>${T("Batterij")}:</b> ${doel}</li>`, ev3Here ? `<li>🚗 <b>Kia EV3:</b> ${ev3}</li>` : "", buzzHere ? `<li>🚐 <b>VW ID.Buzz:</b> ${buzz}</li>` : ""].join(""));

  fill("t_batterij", [
    tile("SoC", pct(w.solis.soc)), tile("Doel (batt + P1)", W(sp)), tile("Werkelijk", W(batW)),
    tile("Bias", Wf(m.batterij_setpoint_bias)), tile("Sturing bezig", onoff(m.soluna_absorptie_bezig), "", m.soluna_absorptie_bezig ? "on" : ""),
    tile("Volgende update", next), tile("Commando", RC[w.solis.rc_mode] || "–"),
    tile("Geladen vandaag", kWh(d.batterij_geladen_dag_kwh)), tile("Ontladen vandaag", kWh(d.batterij_ontladen_dag_kwh)),
  ].join(""));
  fill("t_net", [
    tile("Net (W)", Wf(p1)), tile("PV-overschot netto", Wf(d.pv_overschot_beschikbaar_w)), tile("PV-overschot bruto", Wf(d.pv_bruto_overschot_w)),
    tile("L1", amp(w.p1.phase_a[0])), tile("L2", amp(w.p1.phase_a[1])), tile("L3", amp(w.p1.phase_a[2])),
    tile("Maandpiek-doel", num(d.capaciteit_maandtarget_kw, 2) + " kW"), tile("Piekrisico", risk ? "aan" : "uit", "", risk ? "alarm" : ""),
    tile("Guard-interventies", num(m.peak_guard_interventies)),
  ].join(""));
  const fc = S.forecastNow;
  fill("t_zon", [
    tile("Totaal nu", W(pv)), tile("Solis", W(w.solis.pv_power_w)), tile("Omnik", W(w.omnik.power_w)),
    tile("Forecast nu", fc === undefined ? "–" : W(fc), fc === undefined ? "geen forecastbron" : ""),
    tile("PV vandaag (Solis + Omnik)", kWh(d.pv_energie_dag_kwh)),
  ].join(""));
  fill("t_evs", [
    tile("EV3 kabel", esc(w.cable.status || "–")), tile("EV3 stroom", amp(w.cable.set_current_a, 0)), tile("EV3 accu", pct(w.ev3.soc)),
    tile("Buzz stekker", esc(w.evse.plug || "–")), tile("Buzz status", esc(w.evse.state || "–")), tile("Buzz override", amp(w.evse.override_a)),
    tile("Buzz pilot", amp(w.evse.charge_current_a)), tile("Buzz accu", pct(w.buzz.soc)),
    tile("EV3 AC-limiet", pct(w.ev3.ac_limit)), tile("EV3 kilometerstand", km(w.ev3.odometer_km)),
    tile("Buzz laaddoel", pct(w.buzz.target_soc)), tile("Buzz kilometerstand", km(w.buzz.odometer_km)),
  ].join(""));
  const aircoSrc = esc((s.sources || {}).airco || "–");
  fill("t_airco", [
    tile("Airco beneden", onoff(w.airco.beneden_on), aircoSrc, w.airco.beneden_on ? "on" : ""),
    tile("Airco zolder", onoff(w.airco.zolder_on), aircoSrc, w.airco.zolder_on ? "on" : ""),
    tile("Dump bezig", onoff(m.airco_dump_sessie_bezig)),
  ].join(""));
  const aircoCtl = entity("world_load_airco_beneden_on");
  $("airco_note").textContent = aircoCtl && aircoCtl.control
    ? T("Let op: de airco-dump en de piekwacht schakelen de aircos ook.")
    : T("Schakelen kan als een apparaat de aircos bestuurt (control = \"load:airco\", read_only = false) en niet in shadow-modus.");
  fill("t_dag", [
    tile("Zelfverbruik (van PV)", pct(d.zelfverbruik_vandaag_pct)), tile("Zelfvoorziening (van verbruik)", pct(d.zelfvoorziening_vandaag_pct)),
    tile("Verbruik", kWh(d.huisverbruik_dag_kwh)), tile("PV productie", kWh(d.pv_energie_dag_kwh)), tile("PV zelf gebruikt", kWh(d.zelfverbruik_vandaag_kwh)),
    tile("Injectie", kWh(d.injectie_dag_kwh)), tile("Netimport", kWh(d.netimport_dag_kwh)),
    ...(nb ? [] : [tile("Batterij geladen", kWh(d.batterij_geladen_dag_kwh)), tile("Batterij ontladen", kWh(d.batterij_ontladen_dag_kwh))]),
  ].join(""));
  fill("t_nacht", [
    ...(nb ? [] : [tile("Nachtdekking", pct(m.nachtdekking))]), tile("Duur donkere periode", num(m.nacht_duur_uren, 1) + " u"),
    ...(nb ? [] : [tile("Nacht uit batterij", kWh(m.nacht_batterij_kwh))]), tile("Nacht uit net", kWh(m.nacht_import_kwh)),
    tile("Airco beneden", onoff(w.airco.beneden_on), "", w.airco.beneden_on ? "on" : ""), tile("Airco zolder", onoff(w.airco.zolder_on), "", w.airco.zolder_on ? "on" : ""),
    tile("Dump bezig", onoff(m.airco_dump_sessie_bezig)), tile("Vriezer", W(w.vriezer.power_w), w.vriezer.switch_on === null ? "" : onoff(w.vriezer.switch_on)),
  ].join(""));

  // Capaciteitstarief
  fill("t_kwartier", [
    tile("Mag nu", Wf(d.kwartier_toegestaan_vermogen_w)), tile("Projectie", Wf(d.kwartiervermogen_projectie_w)),
    tile("Verbruikt dit kwartier", kWh(d.kwartier_import_kwh, 3)),
  ].join(""));
  renderQuarterText(s);
  Charts.gauge($("g_maandpiek"), { value: w.p1.month_peak_w, min: 0, max: 8000, unit: "W", label: "Maandpiek (Fluvius)",
    severity: [{ from: 0, color: "#43a047" }, { from: 2500, color: "#f9a825" }, { from: 4000, color: "#e53935" }] });
  fill("t_maand", [
    tile("Maandpiek × tarief", eur(d.capaciteitstarief_deze_maand_eur)), tile("Gefactureerde piek (12 mnd gem.)", num(d.gefactureerde_piek_kw, 3) + " kW"),
    tile("Op factuur deze maand", eur(d.capaciteitstarief_factuur_deze_maand_eur)),
  ].join(""));
  fill("t_maand2", [
    tile("Doel rest van maand", num(d.capaciteit_maandtarget_kw, 2) + " kW"), tile("Projectie lopend kwartier", Wf(d.kwartiervermogen_projectie_w)),
    tile("Vastgelegd lopend kwartier", num(d.kwartiervermogen_vastgelegd_kw, 3) + " kW"), tile("Import dit kwartier", kWh(d.kwartier_import_kwh, 3)),
  ].join(""));
  renderPeakHistory(s);
  fill("t_fluvius", [
    tile("Maandpiek Fluvius", nil(w.fluvius.peak_kw) ? "–" : num(w.fluvius.peak_kw, 3) + " kW"), tile("Maandpiek lokaal (P1)", Wf(w.p1.month_peak_w)),
    tile("Verschil", Wf(d.piek_verschil_fluvius_vs_p1_w)),
    ...(devOf(s, "fluvius") ? [tile("Piekkwartier", when(devVal(devOf(s, "fluvius"), "peak_start")), "Mijn Fluvius · " + esc(devOf(s, "fluvius")))] : []),
  ].join(""));
  renderFluviusMonths(s);

  // Prijzen
  const pr = S.prices || {};
  fill("t_prijs", [
    tile("Afname dit kwartier (all-in)", eurkwh(d.stroomprijs_nu)), tile("Volgend kwartier", eurkwh(d.kwartierprijs_volgend)),
    tile("Injectie dit kwartier", eurkwh(w.prices.injectie_now)), tile("Niveau", esc(d.stroomprijs_niveau || "–")),
    tile("Positie in dagrange", pct(w.prices.afname_pct_in_range, 1)),
    tile("Laagste kwartier", eurkwh(pr.laagste && pr.laagste.price)), tile("Tijd laagste", pr.laagste ? hhmm(pr.laagste.t * 1000) : "–"),
    tile("Hoogste kwartier", eurkwh(pr.hoogste && pr.hoogste.price)), tile("Tijd hoogste", pr.hoogste ? hhmm(pr.hoogste.t * 1000) : "–"),
  ].join(""));
  fill("t_vast", [
    tile("Abonnement + databeheer + energiefonds", eur(d.vaste_kosten_per_maand_eur), "per maand"),
    tile("Capaciteitstarief (maandpiek × tarief)", eur(d.capaciteitstarief_deze_maand_eur)),
    tile("Capaciteitstarief op factuur (12 mnd gem.)", eur(d.capaciteitstarief_factuur_deze_maand_eur)),
  ].join(""));
  fill("t_geld", [tile("Kost-snelheid nu (€/u)", eur(d.kost_per_uur_eur, 3)), tile("Energiekost vandaag", eur(d.energiekost_vandaag_eur))].join(""));

  // Batterij & Solis
  Charts.gauge($("g_soc"), { value: w.solis.soc, min: 0, max: 100, unit: "%", label: "SOC",
    severity: [{ from: 0, color: "#e53935" }, { from: 15, color: "#f9a825" }, { from: 30, color: "#43a047" }] });
  fill("t_bstatus", [
    tile("Beschikbare energie", kWh(d.batterij_beschikbare_energie_kwh)), tile("Vermogen (− = laden)", W(batW)),
    tile("Doel (setpoint)", W(sp)), tile("Vandaag geladen", kWh(d.batterij_geladen_dag_kwh)), tile("Vandaag ontladen", kWh(d.batterij_ontladen_dag_kwh)),
    tile("Batterij → auto geleverd", kWh(d.batterij_naar_auto_geleverd_kwh)),
  ].join(""));
  fill("t_solis", [
    tile("Directe sturing", RC[w.solis.rc_mode] || "–"), tile("Laadvermogen (RC)", Wf(w.solis.rc_charge_w)), tile("Ontlaadvermogen (RC)", Wf(w.solis.rc_discharge_w)),
    tile("Time-out (RC)", nil(w.solis.rc_timeout_min) ? "–" : num(w.solis.rc_timeout_min) + " min"),
    tile("Netladen toegestaan", onoff(w.solis.allow_grid_charge)), tile("Injectielimiet", onoff(w.solis.feed_in_limit_on), Wf(w.solis.backflow_w)),
    tile("Solis offline", d.solis_offline ? "ja" : "nee", "", d.solis_offline ? "alarm" : ""), tile("Dispatcher off-target", d.dispatcher_off_target ? "ja" : "nee", "", d.dispatcher_off_target ? "alarm" : ""),
  ].join(""));

  // EV laden
  const ev3km = kmSince(w.ev3.odometer_km, st.ev3_odometer_start), buzzkm = kmSince(w.buzz.odometer_km, st.buzz_odometer_start);
  const ev3dev = devOf(s, "ev3"), buzzdev = devOf(s, "buzz");
  fill("t_ev3", [
    tile("Batterij", pct(w.ev3.soc)), tile("Stekker", nil(w.ev3.plugged) ? "–" : (w.ev3.plugged ? "verbonden" : "los")),
    tile("AC-limiet", pct(w.ev3.ac_limit)), tile("Kilometerstand", km(w.ev3.odometer_km), T("sinds start") + " " + km(ev3km)),
    tile("Laadvermogen", num(d.ev3_laadvermogen_totaal_kw, 2) + " kW", T("zon") + " " + num(d.ev3_laadvermogen_zon_kw, 2) + " · " + T("net") + " " + num(d.ev3_laadvermogen_net_kw, 2)),
    tile("Geladen vandaag", kWh(d.ev3_laadenergie_dag_kwh), T("maand") + " " + kWh(d.ev3_laadenergie_maand_kwh, 1)),
    ...(ev3dev ? [tile("Laadt", onoff(devVal(ev3dev, "charging"))), tile("DC-limiet", pct(devVal(ev3dev, "dc_limit"))),
      tile("Laatste update", when(devVal(ev3dev, "last_updated")), "Kia Connect · " + esc(ev3dev))] : []),
  ].join(""));
  fill("t_buzz", [
    tile("Batterij", pct(w.buzz.soc)), tile("Doel-SOC", pct(w.buzz.target_soc)), tile("Kabel", esc(w.evse.plug || "–")),
    tile("Kilometerstand", km(w.buzz.odometer_km), T("sinds start") + " " + km(buzzkm)),
    tile("Laadvermogen", num(d.buzz_laadvermogen_totaal_kw, 2) + " kW", T("zon") + " " + num(d.buzz_laadvermogen_zon_kw, 2) + " · " + T("net") + " " + num(d.buzz_laadvermogen_net_kw, 2)),
    tile("Geladen vandaag", kWh(d.buzz_laadenergie_dag_kwh), T("maand") + " " + kWh(d.buzz_laadenergie_maand_kwh, 1)),
    ...(buzzdev ? [tile("Laatste voertuigupdate", when(devVal(buzzdev, "captured_at")), "EU Data Act · " + esc(buzzdev))] : []),
  ].join(""));
  renderEv3Control(s);
  fill("t_kabel", [
    tile("Laadstroom", amp(w.cable.set_current_a, 0), "max " + amp(w.cable.set_current_max_a, 0)), tile("Vermogen", Wf(d.ev_laadkabel_vermogen_w)),
    tile("Stroom", amp(w.cable.current_a)), tile("Spanning", nil(w.cable.voltage_v) ? "–" : num(w.cable.voltage_v) + " V"),
    tile("Status", esc(w.cable.status || "–")),
  ].join(""));
  fill("t_evse", [
    tile("Modus", esc(w.evse.mode || "–")), tile("Override stroom", amp(w.evse.override_a), "max " + amp(w.evse.override_max_a, 0)),
    tile("Laadvermogen (→ Buzz)", num(d.ev_laadvermogen_smartevse_kw, 3) + " kW"), tile("Laadstroom (pilot)", amp(w.evse.charge_current_a)),
    tile("Status", esc(w.evse.state || "–"), "pilot " + esc(w.evse.state_id || "–")), tile("Auto verbonden", esc(w.evse.plug || "–")),
  ].join(""));
  fill("t_evactief", [
    tile("Actieve laadauto", d.ev_actieve_laadauto === "Ev3" ? "EV3" : "Buzz"),
    tile("EV-laadvermogen zon", num(d.ev_laadvermogen_zon_kw, 2) + " kW"), tile("EV-laadvermogen net", num(d.ev_laadvermogen_net_kw, 2) + " kW"),
    tile("Benzineprijs", eur(st.ev_benzineprijs) + "/L"),
  ].join(""));
  const carCost = (c) => [
    tile("Zon geladen (kWh)", kWh(d[c + "_laadenergie_zon_kwh"], 1)), tile("Net geladen (kWh)", kWh(d[c + "_laadenergie_net_kwh"], 1)),
    tile("Zon-aandeel", pct(d[c + "_laad_zonaandeel_pct"])), tile("Netkost totaal", eur(d[c + "_laadkost_net_eur"])),
    tile("Netkost deze maand", eur(d[c + "_laadkost_maand_eur"]), T("vandaag") + " " + eur(d[c + "_laadkost_dag_eur"])),
    tile("Kost/100km (EV)", eur(d[c + "_laadkost_per_100km_eur"])), tile("Kost/100km (ICE)", eur(d[c + "_ice_kost_per_100km_eur"])),
    tile("Besparing/100km", eur(d[c + "_besparing_per_100km_eur"])), tile("Verbruik/100km", kWh(d[c + "_verbruik_per_100km_kwh"], 1)),
    tile("Kost-snelheid nu", eur(d[c + "_laadkost_per_uur_eur"], 3) + T("/u")),
  ].join("");
  fill("t_ev3kost", carCost("ev3"));
  fill("t_buzzkost", carCost("buzz"));

  // Systeem
  const h = S.hist;
  fill("t_sys", [
    tile("Home Assistant", s.ha ? (s.ha.connected ? "verbonden" : "niet verbonden") : "niet gebruikt", s.ha ? s.ha.entities + " " + T("entiteiten") : ""),
    tile("MQTT", s.mqtt ? (s.mqtt.connected ? "verbonden" : "niet verbonden") : "niet gebruikt", s.mqtt ? s.mqtt.events + " " + T("berichten") : ""),
    tile("Bronnen", esc([...new Set(Object.values(s.sources))].join(", ") || "–")),
    tile("Actuatoren", esc([...new Set(Object.values(s.actuators))].join(", ") || "–")),
    tile("Statusbestanden", num(s.persist_writes) + " " + T("keer geschreven")),
    ...Object.entries(s.drivers || {}).map(([k, v]) => {
      // A device that signs in shows its sign-in state while that is not ok ("aanmelden nodig":
      // `energie credentials set <id>` on the host).
      const a = v.auth && v.auth.state !== "ok" ? v.auth : null;
      const sub = a ? esc(a.state + (a.last_result ? ": " + a.last_result : "")) : (v.last_error ? esc(v.last_error) : v.reads_ok + " " + T("metingen"));
      return tile(k, a ? "aanmelden" : (v.available ? "ok" : "onbereikbaar"), sub, a || !v.available ? "alarm" : "");
    }),
    ...(h ? h.tiers.map((t) => tile(T("Historiek") + " " + t.name, num(t.rows) + " " + T("rijen"), t.first ? T("sinds") + " " + new Date(t.first * 1000).toLocaleDateString(LOCALE) : "leeg"))
      .concat([tile("Historiek geschreven", num(h.stats.bytes_written / 1024, 1) + " kB", h.stats.write_errors ? h.stats.write_errors + " " + T("fouten") : num(h.stats.rows_written) + " " + T("rijen"))]) : [tile("Historiek", "uit")]),
  ].join(""));

  for (const o of document.querySelectorAll("output[data-d]")) {
    const v = d[o.dataset.d];
    o.textContent = nil(v) ? "–" : num(v, Number(o.dataset.dec || 0)) + (o.dataset.unit ? " " + o.dataset.unit : "");
  }
}

function renderQuarterText(s) {
  const d = s.derived;
  const tgt = (d.capaciteit_maandtarget_kw ?? 2.5) * 1000, used = d.kwartier_import_kwh || 0, budget = tgt * 0.25 / 1000;
  const now = new Date(S.now);
  let rem = d.kwartier_next_reset ? (Date.parse(d.kwartier_next_reset) - S.now) / 1000 : 900 - (now.getMinutes() % 15) * 60 - now.getSeconds();
  rem = Math.min(Math.max(rem, 0), 900);
  const elapsed = 900 - rem, proj = d.kwartiervermogen_projectie_w || 0, allowed = d.kwartier_toegestaan_vermogen_w || 0;
  const expected = budget * elapsed / 900, pace = expected > 0.002 ? used / expected * 100 : 100;
  const mm = Math.floor(elapsed / 60), ss = String(Math.floor(elapsed % 60)).padStart(2, "0");
  const over = num((used - expected) * 1000), krediet = num((expected - used) * 1000), rest = num(Math.max(budget - used, 0), 3), remMin = num(rem / 60, 1);
  const tempo = I18N.en
    ? (pace > 105 ? `🔴 <b>Pace ${num(pace)}%</b>: ahead of the clock by ${over} Wh, so the controller reduces to ${num(allowed)} W to land the quarter average on ${num(tgt)} W.`
      : pace < 95 ? `🟢 <b>Pace ${num(pace)}%</b>: behind the clock with ${krediet} Wh of credit, so ${num(allowed - tgt)} W more than the target is allowed right now.`
      : `🟡 <b>Pace ${num(pace)}%</b>: on schedule.`)
    : (pace > 105 ? `🔴 <b>Tempo ${num(pace)}%</b> — sneller dan de klok: ${over} Wh te veel, dus de regelaar neemt terug naar ${num(allowed)} W om het kwartiergemiddelde nog op ${num(tgt)} W te laten landen.`
      : pace < 95 ? `🟢 <b>Tempo ${num(pace)}%</b> — trager dan de klok: ${krediet} Wh krediet opgebouwd, daarom mag er nu ${num(allowed - tgt)} W méér dan het target.`
      : `🟡 <b>Tempo ${num(pace)}%</b> — op schema.`);
  fill("md_kwartier", I18N.en
    ? `<h3>⏱️ Quarter ${mm}:${ss} / 15:00</h3>
    <p><b>Budget</b> ${num(budget, 3)} kWh · <b>used</b> ${num(used, 3)} kWh · <b>left</b> ${rest} kWh in ${remMin} min</p>
    <p><b>Allowed now</b> ${num(allowed)} W · <b>projection</b> ${num(proj)} W of ${num(tgt)} W</p>
    <p>${tempo}</p>
    <p class="note">The controller aims for ${num(tgt - 100)} W · the peak guard steps in above ${num(tgt - 25)} W · Fluvius bills at ${num(tgt)} W</p>`
    : `<h3>⏱️ Kwartier ${mm}:${ss} / 15:00</h3>
    <p><b>Budget</b> ${num(budget, 3)} kWh · <b>verbruikt</b> ${num(used, 3)} kWh · <b>rest</b> ${rest} kWh in ${remMin} min</p>
    <p><b>Mag nu</b> ${num(allowed)} W · <b>projectie</b> ${num(proj)} W van ${num(tgt)} W</p>
    <p>${tempo}</p>
    <p class="note">Regelaar mikt op ${num(tgt - 100)} W · piekwacht grijpt in boven ${num(tgt - 25)} W · Fluvius rekent af op ${num(tgt)} W</p>`);
}

function renderPeakHistory(s) {
  const now = new Date(S.now), nu = now.getFullYear() * 12 + now.getMonth();
  const deze = Math.max(2.5, (s.world.p1.month_peak_w || 0) / 1000);
  const ym = (i) => `${String(Math.floor(i / 12)).padStart(4, "0")}-${String(i % 12 + 1).padStart(2, "0")}`;
  const fl = fluviusPeaks(s), fkw = (k) => fl[k] === undefined ? "–" : num(fl[k], 3) + " kW";
  const rows = [];
  for (const e of (s.memory.capaciteit_piekhistoriek || "").split(",")) {
    if (!e.includes(":")) continue;
    const [k, v] = e.split(":");
    const idx = parseInt(k.slice(0, 4), 10) * 12 + parseInt(k.slice(5, 7), 10) - 1, kw = parseFloat(v);
    if (!(nu - idx >= 1 && nu - idx <= 11)) continue;
    rows.push(`<tr><td>${esc(k)}</td><td class="n">${num(kw, 3)} kW${kw < 2.5 ? ` (→ ${num(2.5, 1)})` : ""}</td><td class="n">${fkw(ym(idx))}</td><td>${ym(idx + 11)}</td></tr>`);
  }
  rows.push(`<tr><td><b>${ym(nu)}</b> (${T("lopend")})</td><td class="n"><b>${num(deze, 3)} kW</b></td><td class="n">${fkw(ym(nu))}</td><td>${ym(nu + 11)}</td></tr>`);
  fill("piekhist", rows.join(""));
  fill("piekgem", I18N.en ? `<b>Average = ${num(s.derived.gefactureerde_piek_kw, 3)} kW</b> → ${eur(s.derived.capaciteitstarief_factuur_deze_maand_eur)} incl. VAT on this month's bill.` : `<b>Gemiddelde = ${num(s.derived.gefactureerde_piek_kw, 3)} kW</b> → ${eur(s.derived.capaciteitstarief_factuur_deze_maand_eur)} incl. btw op de factuur van deze maand.`);
}

// The monthly peaks of the `capacity_peak` device (`month_MM_peak_kw`, named "Piek YYYY-MM"),
// by "YYYY-MM".
function fluviusPeaks(s) {
  const dev = devOf(s, "fluvius"), out = {};
  for (const e of dev ? entities : []) {
    const m = e.group === dev && /^month_\d\d_peak_kw$/.test(e.key) && /(\d{4}-\d{2})/.exec(e.name);
    if (m && !nil(e.value)) out[m[1]] = e.value;
  }
  return out;
}

// The local P1 peaks by "YYYY-MM": the piekhistoriek plus the running month.
function localPeaks(s) {
  const out = {};
  for (const e of (s.memory.capaciteit_piekhistoriek || "").split(",")) {
    if (e.includes(":")) out[e.split(":")[0]] = parseFloat(e.split(":")[1]);
  }
  const now = new Date(S.now);
  if (!nil(s.world.p1.month_peak_w)) out[`${now.getFullYear()}-${String(now.getMonth() + 1).padStart(2, "0")}`] = s.world.p1.month_peak_w / 1000;
  return out;
}

function renderFluviusMonths(s) {
  const fl = fluviusPeaks(s), lo = localPeaks(s), months = Object.keys(fl).sort().reverse();
  $("fluviusbox").classList.toggle("hidden", months.length === 0);
  const start = devVal(devOf(s, "fluvius"), "peak_start");
  fill("fluviusmaand", months.map((k, i) => {
    const diff = lo[k] === undefined ? null : fl[k] - lo[k];
    return `<tr><td>${esc(k)}</td><td class="n">${num(fl[k], 3)} kW</td><td class="n">${lo[k] === undefined ? "–" : num(lo[k], 3) + " kW"}</td>`
      + `<td class="n">${nil(diff) ? "–" : num(diff * 1000) + " W"}</td><td>${i === 0 ? when(start) : ""}</td></tr>`;
  }).join(""));
}

// The EV3's AC charging limit, when its device makes it settable.
function renderEv3Control(s) {
  const dev = devOf(s, "ev3"), e = dev && entity(dev + "_ac_limit"), form = $("ev3_ctl");
  form.classList.toggle("hidden", !(e && e.control));
  if (!e || !e.control || form.dataset.for === e.id) return;
  const c = e.control, opts = [];
  for (let v = c.min ?? 0; v <= (c.max ?? 100); v += c.step || 1) opts.push(`<option value="${v}">${num(v)} %</option>`);
  form.innerHTML = `<label class="row"><span>${T("AC-laadlimiet instellen")}</span><select data-entity="${esc(e.id)}">${opts.join("")}</select></label>`;
  form.dataset.for = e.id;
}

// Entity controls (`form.entctl [data-entity]`): enabled while the entity is settable, synced
// from its value unless the form has the focus, posted to /api/entities/<id> on change.
function syncEntityControls() {
  for (const el of document.querySelectorAll("form.entctl [data-entity]")) {
    if (el.form.contains(document.activeElement)) continue;
    const e = entity(el.dataset.entity);
    el.disabled = S.demo || !(e && e.control);
    if (!e || nil(e.value)) continue;
    if (el.type === "checkbox") el.checked = !!e.value;
    else el.value = String(e.value);
  }
}
for (const form of document.querySelectorAll("form.entctl")) {
  form.addEventListener("change", async (ev) => {
    const el = ev.target;
    const value = el.type === "checkbox" ? el.checked : Number(el.value);
    const r = await fetch("/api/entities/" + encodeURIComponent(el.dataset.entity), { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ value }) });
    const msg = form.nextElementSibling;
    if (msg) msg.textContent = r.ok ? T("Verstuurd.") : T("Niet verstuurd:") + " " + await r.text();
  });
}

// ------------------------------------------------------------------ power flow
const FLOW_BASE_S = 2; // the `flowdash` duration in app.css
const FLOW = {
  solar: [180, 48], grid: [48, 160], home: [312, 160], battery: [180, 272], ev3: [312, 44], buzz: [312, 276],
};
function renderFlow(s) {
  const host = $("flow");
  if (!host) return;
  const w = s.world, d = s.derived;
  const pv = Math.max(0, d.pv_vermogen_totaal_w || 0), grid = w.p1.power_w || 0, batt = w.solis.battery_power_w || 0;
  const imp = Math.max(0, grid), exp = Math.max(0, -grid), chg = Math.max(0, -batt), dis = Math.max(0, batt);
  const solarToBatt = Math.min(pv, chg), gridToBatt = Math.max(0, chg - solarToBatt);
  const solarRest = pv - solarToBatt, solarToGrid = Math.min(solarRest, exp), battToGrid = Math.max(0, exp - solarToGrid);
  const solarToHome = Math.max(0, solarRest - solarToGrid), battToHome = Math.max(0, dis - battToGrid), gridToHome = Math.max(0, imp - gridToBatt);
  const ev3 = d.ev_laadkabel_vermogen_w || 0, buzz = d.ev_laadvermogen_smartevse_w || 0;
  const home = d.huisverbruik_vermogen_w || 0, homeNet = Math.max(0, home - ev3 - buzz);
  const total = Math.max(1, solarToHome + solarToGrid + solarToBatt + gridToHome + gridToBatt + battToHome + battToGrid + ev3 + buzz);
  const P = FLOW;
  const flows = [
    ["M180,82 Q180,160 278,160", solarToHome, "#ff9800"],
    ["M180,82 Q180,160 82,160", solarToGrid, "#ff9800"],
    ["M180,82 L180,238", solarToBatt, "#ff9800"],
    ["M82,160 L278,160", gridToHome, "#488fc2"],
    ["M82,160 Q180,160 180,238", gridToBatt, "#488fc2"],
    ["M180,238 Q180,160 278,160", battToHome, "#3FC9A0"],
    ["M180,238 Q180,160 82,160", battToGrid, "#3FC9A0"],
    ["M312,126 L312,74", ev3, "#9B78F5"],
    ["M312,194 L312,246", buzz, "#26c6da"],
  ];
  const dur = (v) => (6 - Math.min(1, v / total) * (6 - 0.9)).toFixed(1);
  const node = (key, r, color, name, value, sub, off) => {
    const [x, y] = P[key];
    const above = key === "battery" || key === "buzz" ? r + 14 : -r - 6;
    return `<g class="node${off ? " off" : ""}"><circle cx="${x}" cy="${y}" r="${r}" stroke="${color}"/>` +
      `<text x="${x}" y="${y + 5}" text-anchor="middle">${value}</text>` +
      (sub ? `<text class="sub" x="${x}" y="${y + 18}" text-anchor="middle">${sub}</text>` : "") +
      name.split("\n").map((n, i) => `<text class="name" x="${x}" y="${y + above + i * 13}" text-anchor="middle">${esc(T(n))}</text>`).join("") + "</g>";
  };
  const sp = d.batterij_setpoint_w;
  const doel = sp < 0 ? T("doel laden") + " " + num(-sp) + " W" : sp > 0 ? T("doel ontladen") + " " + num(sp) + " W" : T("doel vasthouden");
  const gridTxt = imp > 0 ? "← " + Wf(imp) : exp > 0 ? "→ " + Wf(exp) : "0 W";
  const airco = [w.airco.beneden_on ? T("beneden") : null, w.airco.zolder_on ? T("zolder") : null].filter(Boolean);
  // The paths stay in the DOM: replacing them would restart every dot's animation on each update.
  if (!host.querySelector("svg")) {
    host.innerHTML = `<svg viewBox="0 0 400 345" role="img" aria-label="${T("Energiestroom")}"><g transform="translate(20,0)">
      ${flows.map(([p]) => `<path class="track" d="${p}"/>`).join("")}
      ${flows.map(([p, , c]) => `<path class="dot" d="${p}" stroke="${c}"/>`).join("")}
      <g class="nodes"></g>
    </g></svg>`;
  }
  host.querySelectorAll("path.dot").forEach((el, i) => {
    const v = flows[i][1];
    el.style.visibility = v > 10 ? "visible" : "hidden";
    if (s.world.no_battery && flows[i][0].includes("180,238")) host.querySelectorAll("path.track")[i].style.display = "none";
    if (!hasDevice(s, "cable") && i === 7) host.querySelectorAll("path.track")[i].style.display = "none";
    if (!hasDevice(s, "evse") && i === 8) host.querySelectorAll("path.track")[i].style.display = "none";
    // The playback rate changes the speed without moving the dots; the CSS duration would.
    for (const a of el.getAnimations()) a.updatePlaybackRate(FLOW_BASE_S / dur(v));
  });
  host.querySelector("g.nodes").innerHTML = `
    ${node("solar", 34, "#ff9800", "PV (Solis + Omnik)", Wf(pv), "", pv <= 0)}
    ${node("grid", 34, "#488fc2", "Net", gridTxt, "", false)}
    ${node("home", 34, "#5B8DEF", "Huis", Wf(homeNet), airco.length ? T("airco") + " " + airco.join(", ") : "", false)}
    ${w.no_battery ? "" : node("battery", 34, "#3FC9A0", T("Batterij") + "\n" + doel, pct(w.solis.soc), Wf(Math.abs(batt)) + (batt > 0 ? " ↑" : batt < 0 ? " ↓" : ""), false)}
    ${hasDevice(s, "cable") ? node("ev3", 26, "#9B78F5", "Kia EV3 (laadkabel)", Wf(ev3), "", ev3 <= 0) : ""}
    ${hasDevice(s, "evse") ? node("buzz", 26, "#26c6da", "VW ID.Buzz (SmartEVSE)", Wf(buzz), "", buzz <= 0) : ""}`;
}

// ------------------------------------------------------------------ data
async function hist(ids, from, to, step) {
  const q = new URLSearchParams({ series: ids.join(","), from: Math.floor(from / 1000), to: Math.ceil(to / 1000), step });
  const r = await fetch("/api/history?" + q);
  if (!r.ok) throw new Error(await r.text());
  const j = await r.json();
  const out = {};
  ids.forEach((id, i) => { out[id] = j.t.map((t, k) => [t * 1000, j.series[i].v[k]]); });
  return out;
}
const scale = (data, f) => data.map(([t, v]) => [t, nil(v) ? null : f(v)]);
const kw = (data) => scale(data, (v) => v / 1000);
async function loadPrices() {
  const r = await fetch("/api/prices");
  if (!r.ok) return;
  S.prices = await r.json();
}
async function loadHistMeta() {
  const r = await fetch("/api/history");
  S.hist = r.ok ? await r.json() : null;
}
const priceSeries = () => {
  const p = S.prices || { afname: [], injectie: [] };
  return { afname: p.afname.map(([t, v]) => [t * 1000, v]), injectie: p.injectie.map(([t, v]) => [t * 1000, v]) };
};

// Flow chart period, as in HA's `input_select.grafiek_periode`.
function flowSpan(sel) {
  const n = S.now;
  switch (sel) {
    case "12h": return { from: n - 12 * HOUR, to: n, step: 300, title: "laatste 12 uur" };
    case "24h": return { from: n - 24 * HOUR, to: n, step: 600, title: "laatste 24 uur" };
    case "gisteren": return { from: startOfDay(n, -1), to: startOfDay(n), step: 600, title: "gisteren" };
    case "7d": return { from: startOfDay(n, -6), to: startOfDay(n, 1), step: 3600, title: "7 dagen" };
    default: return { from: n - 6 * HOUR, to: n, step: 300, title: "laatste 6 uur" };
  }
}
function fcSpan(sel) {
  const n = S.now;
  if (sel === "morgen") return { from: startOfDay(n), to: startOfDay(n, 2), title: "vandaag + morgen" };
  if (sel === "nu12") { const to = startOfHour(n) + 13 * HOUR; return { from: to - 24 * HOUR, to, title: "nu ±12u" }; }
  return { from: startOfDay(n), to: startOfDay(n, 1), title: "vandaag" };
}

const CHARTS = {
  c_flow: { tab: "overzicht", every: 60, async load() {
    const sp = flowSpan($("flowperiod").value);
    $("flowtitle").textContent = T("Energiestroom") + " — " + T(sp.title);
    const h = await hist(["pv_w", "batterij_w", "p1_w", "huis_basis_w", "ev3_w", "buzz_w"], sp.from, sp.to, sp.step);
    const L = (id, name, color, type = "line", opacity) => ({ name, color, type, opacity, data: kw(h[id]), unit: "kW", dec: 1 });
    const snap = S.snap, nb = snap.world.no_battery;
    return { from: sp.from, to: sp.to, now: S.now, bucket: sp.step * 1000, axes: [{ dec: 1, title: "kW" }], series: [
      L("pv_w", "Zon", "#ff9800", "area", 0.3), nb ? null : L("batterij_w", "Batterij (+ontl/−laad)", "#3FC9A0"), L("p1_w", "Net (+afn/−inj)", "#9e9e9e"),
      L("huis_basis_w", "Huis (basis)", "#5B8DEF"), hasDevice(snap, "cable") ? L("ev3_w", "EV3", "#9B78F5") : null, hasDevice(snap, "evse") ? L("buzz_w", "Buzz", "#26c6da") : null].filter(Boolean) };
  } },
  c_fc: { tab: "overzicht", every: 60, async load() {
    const sp = fcSpan($("fcperiod").value);
    $("fctitle").textContent = T("Zon & prijs") + " — " + T(sp.title);
    const h = await hist(["pv_w", "pv_forecast_w"], sp.from, sp.to, 900);
    const p = priceSeries();
    const hasFc = h.pv_forecast_w.some(([, v]) => !nil(v));
    const nowFc = h.pv_forecast_w.find(([t]) => t <= S.now && S.now < t + 900000);
    S.forecastNow = hasFc && nowFc && !nil(nowFc[1]) ? nowFc[1] : undefined;
    $("fcnote").textContent = hasFc ? "" : T(" Er is nog geen PV-forecastbron: de forecastlijn verschijnt zodra er een is.");
    const series = [{ name: "PV werkelijk", color: "#ff9800", type: "area", opacity: 0.3, data: kw(h.pv_w), unit: "kW", dec: 1 }];
    if (hasFc) series.push({ name: "PV forecast", color: "#1976d2", data: kw(h.pv_forecast_w), unit: "kW", dec: 1 });
    series.push({ name: "Afname", color: "#e53935", type: "step", axis: 1, data: p.afname, unit: "€/kWh", dec: 3 },
      { name: "Injectie", color: "#43a047", type: "step", axis: 1, width: 1, opacity: 0.7, data: p.injectie, unit: "€/kWh", dec: 3 });
    return { from: sp.from, to: sp.to, now: S.now, bucket: 900000, axes: [{ min: 0, dec: 1, title: "kW" }, { dec: 3, title: "€/kWh" }], series };
  } },
  c_quarter: { tab: "capaciteit", every: 10, async load() {
    const from = quarterStart(S.now), to = from + 900000;
    const h = await hist(["kwartier_gemiddelde_w", "kwartier_projectie_w", "afname_w", "maandtarget_kw:max"], from, to, 30);
    return { from, to, now: S.now, bucket: 30000, height: 200, axes: [{ min: 0, dec: 0, title: "W" }], series: [
      { name: "Gemiddelde dit kwartier", color: "#2196f3", width: 3, data: h.kwartier_gemiddelde_w, unit: "W", dec: 0 },
      { name: "Projectie", color: "#9c27b0", data: h.kwartier_projectie_w, unit: "W", dec: 0 },
      { name: "Actueel (afname)", color: "#607d8b", type: "area", width: 1, opacity: 0.25, data: h.afname_w, unit: "W", dec: 0 },
      { name: "Target (Fluvius)", color: "#f44336", type: "step", data: scale(h["maandtarget_kw:max"], (v) => v * 1000), unit: "W", dec: 0 }] };
  } },
  c_pct: { tab: "capaciteit", every: 60, async load() {
    const from = startOfHour(S.now) - 11 * HOUR, to = from + 12 * HOUR;
    const h = await hist(["kwartier_vastgelegd_kw:max"], from, to, 900);
    const t = S.snap.derived.capaciteit_maandtarget_kw;
    return { from, to, now: S.now, bucket: 900000, states: false, axes: [{ min: 0, max: 120, dec: 0, title: "% van target" }],
      annotations: [{ y: 100, color: "#f44336", label: "target (100%)" }],
      series: [{ name: "Kwartier behaald", color: "#2196f3", type: "column", data: scale(h["kwartier_vastgelegd_kw:max"], (v) => t > 0 ? v / t * 100 : 0), unit: "%", dec: 0 }] };
  } },
  c_kw24: { tab: "capaciteit", every: 60, async load() {
    const from = startOfHour(S.now) - 23 * HOUR, to = from + 24 * HOUR;
    const h = await hist(["kwartier_vastgelegd_kw:max", "maandtarget_kw:max"], from, to, 900);
    return { from, to, now: S.now, bucket: 900000, axes: [{ min: 0, dec: 0, title: "W" }], series: [
      { name: "Kwartiergemiddelde", color: "#3f51b5", type: "column", data: scale(h["kwartier_vastgelegd_kw:max"], (v) => v * 1000), unit: "W", dec: 0 },
      { name: "Target", color: "#f44336", type: "step", data: scale(h["maandtarget_kw:max"], (v) => v * 1000), unit: "W", dec: 0 }] };
  } },
  c_dagpiek: { tab: "capaciteit", every: 300, async load() {
    const to = startOfDay(S.now, 1), from = startOfDay(S.now, -13);
    const h = await hist(["kwartier_vastgelegd_kw:max"], from, to, 86400);
    return { from, to, bucket: DAY, states: false, axes: [{ min: 0, dec: 1, title: "kW" }], annotations: [{ y: 2.5, color: "#f44336", label: num(2.5, 1) + " kW" }],
      series: [{ name: "Dagpiek", color: "#3f51b5", type: "column", data: h["kwartier_vastgelegd_kw:max"], unit: "kW", dec: 3 }] };
  } },
  c_prices: { tab: "prijzen", every: 60, async load() {
    await loadPrices();
    const from = startOfDay(S.now), to = startOfDay(S.now, 2), p = priceSeries();
    const limit = S.snap.settings.goedkoop_laden_prijs_drempel;
    return { from, to, now: S.now, bucket: 900000, onselect: choosePriceHours, axes: [{ dec: 2, title: "€/kWh" }],
      annotations: [{ y: limit, color: "#1f9d55", label: T("Laadlimiet") + " " + num(limit, 2) }], series: [
      { name: "Afname (all-in)", color: "#e53935", type: "column", data: p.afname, unit: "€/kWh", dec: 3, extremas: true, header: false,
        thresholds: [{ value: -1, color: "#1b5e20" }, { value: 0.21, color: "#7cb342" }, { value: 0.26, color: "#f9a825" }, { value: 0.32, color: "#e53935" }] },
      { name: "Injectie", color: "#607d8b", type: "step", data: p.injectie, unit: "€/kWh", dec: 3, header: false }] };
  } },
  c_geld: { tab: "prijzen", every: 60, async load() {
    const from = startOfDay(S.now), to = startOfDay(S.now, 1);
    const h = await hist(["kost_per_uur_eur"], from, to, 900);
    return { from, to, now: S.now, bucket: 900000, axes: [{ dec: 3, title: "€" }, { min: 0, dec: 2, title: "€/kWh" }], series: [
      { name: "€ per kwartier", color: "#e53935", type: "column", data: scale(h.kost_per_uur_eur, (v) => v * 0.25), unit: "€", dec: 3,
        thresholds: [{ value: -10, color: "#1b5e20" }, { value: 0, color: "#9ccc65" }, { value: 0.0001, color: "#e53935" }] },
      { name: "Afname all-in", color: "#1976d2", type: "step", axis: 1, data: priceSeries().afname, unit: "€/kWh", dec: 3 }] };
  } },
  c_kostdag: { tab: "prijzen", every: 300, async load() {
    const to = startOfDay(S.now, 1), from = startOfDay(S.now, -13);
    const h = await hist(["energiekost_vandaag_eur:last"], from, to, 86400);
    return { from, to, bucket: DAY, states: false, axes: [{ dec: 2, title: "€" }],
      series: [{ name: "Netto kost/dag", color: "#3f51b5", type: "column", data: h["energiekost_vandaag_eur:last"], unit: "€", dec: 2 }] };
  } },
  c_batt: { tab: "batterij", every: 60, async load() {
    const to = S.now, from = to - 24 * HOUR;
    const h = await hist(["batterij_w", "soc_pct:last"], from, to, 300);
    return { from, to, now: S.now, bucket: 300000, axes: [{ dec: 1, title: "kW" }, { min: 0, max: 100, dec: 0, title: "%" }], series: [
      { name: "Vermogen (− = laden)", color: "#3f51b5", type: "area", width: 1, opacity: 0.35, data: kw(h.batterij_w), unit: "kW", dec: 2 },
      { name: "SOC", color: "#4caf50", axis: 1, data: h["soc_pct:last"], unit: "%", dec: 0 }] };
  } },
  c_pv: { tab: "batterij", every: 60, async load() {
    const to = S.now, from = to - 24 * HOUR;
    const h = await hist(["solis_pv_w", "omnik_w"], from, to, 600);
    return { from, to, now: S.now, bucket: 600000, axes: [{ min: 0, dec: 1, title: "kW" }], series: [
      { name: "Solis", color: "#ff9800", type: "area", width: 1, opacity: 0.4, data: kw(h.solis_pv_w), unit: "kW", dec: 2 },
      { name: "Omnik", color: "#ffc107", type: "area", width: 1, opacity: 0.4, data: kw(h.omnik_w), unit: "kW", dec: 2 }] };
  } },
};

// A range dragged on the price chart becomes the price limit: charging runs whenever the price
// is below it, so the limit is just above the dearest quarter of the range.
function choosePriceHours(from, to) {
  const box = $("priceselect");
  const slots = (S.prices ? S.prices.afname : []).map(([t, v]) => [t * 1000, v]);
  const picked = slots.filter(([t]) => t + 900000 > from && t < to);
  if (!picked.length) { box.textContent = ""; return; }
  const max = Math.max(...picked.map(([, v]) => v));
  const limit = Math.min(0.6, Math.max(0.05, (Math.floor(max * 100 + 1e-9) + 1) / 100));
  const others = slots.filter(([t, v]) => v < limit && t + 900000 > S.now && !(t + 900000 > from && t < to)).length;
  const span = hhmm(picked[0][0]) + "–" + hhmm(picked[picked.length - 1][0] + 900000);
  const what = I18N.en
    ? `<b>${span}</b>: the dearest quarter costs ${eurkwh(max)}. Charging from the grid then runs whenever the price is below <b>${eurkwh(limit)}</b>${others ? `, which also includes ${others} other upcoming quarters` : ""}.`
    : `<b>${span}</b>: het duurste kwartier kost ${eurkwh(max)}. Laden van het net loopt dan zolang de prijs onder <b>${eurkwh(limit)}</b> ligt${others ? `, ook tijdens ${others} andere komende kwartieren` : ""}.`;
  box.innerHTML = what + `<br><button type="button" id="ps_ok">${T("Toepassen")}</button><button type="button" id="ps_no">${T("Annuleren")}</button>`;
  const done = () => { box.textContent = ""; S.fetched.c_prices = 0; refreshCharts(false); };
  $("ps_no").onclick = done;
  $("ps_ok").disabled = S.demo;
  $("ps_ok").onclick = async () => {
    const net = document.querySelector('select[name="ev_net_niveau"]');
    const max2peak = net && [...net.options].find((o) => o.textContent === T("Max tot piek"));
    const body = { goedkoop_laden_prijs_drempel: limit };
    if (max2peak) body.ev_net_niveau = JSON.parse(max2peak.value);
    const r = await fetch("/api/settings", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body) });
    if (r.ok) done(); else box.append(" " + T("Niet opgeslagen:") + " " + await r.text());
  };
}

function draw(id) {
  const cfg = S.cache[id], host = $(id);
  if (cfg && host && host.offsetParent !== null) Charts.chart(host, cfg);
}

async function refreshCharts(force) {
  if (!S.snap || !S.snap.now) return;
  const wall = Date.now();
  for (const [id, c] of Object.entries(CHARTS)) {
    if (c.tab !== S.tab) continue;
    if (!force && S.fetched[id] && wall - S.fetched[id] < c.every * 1000) continue;
    S.fetched[id] = wall;
    try {
      S.cache[id] = await c.load();
      draw(id);
    } catch (e) {
      $(id).innerHTML = `<p class="note">${T("Geen historiek:")} ${esc(e.message)}</p>`;
    }
  }
}

// ------------------------------------------------------------------ tabs
function showTab(name) {
  if (!document.querySelector(`.view[data-view="${name}"]`) || (name === "batterij" && S.snap && S.snap.world.no_battery)) name = "overzicht";
  S.tab = name;
  for (const v of document.querySelectorAll(".view")) v.classList.toggle("on", v.dataset.view === name);
  for (const a of document.querySelectorAll("#tabs a")) a.classList.toggle("on", a.dataset.tab === name);
  refreshCharts(true);
}
window.addEventListener("hashchange", () => showTab(location.hash.slice(1)));
let resizeTimer = null;
window.addEventListener("resize", () => { clearTimeout(resizeTimer); resizeTimer = setTimeout(() => Object.keys(S.cache).forEach(draw), 150); });
for (const [sel, key] of [["flowperiod", "c_flow"], ["fcperiod", "c_fc"]]) {
  const e = $(sel);
  e.value = store.get("energie." + sel, e.value);
  e.addEventListener("change", () => { store.set("energie." + sel, e.value); S.fetched[key] = 0; refreshCharts(false); });
}

// ------------------------------------------------------------------ controls
const BATTERY_SETTINGS = new Set(["ev_batterij_niveau", "batterij_naar_auto_budget", "soluna_absorptie_actief", "batterij_reservering_actief", "batterij_reservering_w", "batterij_reserve_soc"]);
function syncControls(settings, editable) {
  const nb = !!(S.snap && S.snap.world.no_battery);
  for (const el of document.querySelectorAll("form.controls [name]")) if (BATTERY_SETTINGS.has(el.name)) el.closest(".row").classList.toggle("hidden", nb);
  for (const o of document.querySelectorAll('output[data-d^="batterij_"]')) o.closest(".row").classList.toggle("hidden", nb);
  for (const form of document.querySelectorAll("form.controls")) {
    if (document.activeElement && form.contains(document.activeElement)) continue;
    for (const el of form.elements) {
      if (!el.name) continue;
      const v = settings[el.name];
      if (el.type === "checkbox") el.checked = !!v;
      else if (el.tagName === "SELECT") el.value = JSON.stringify(v);
      else el.value = v;
      el.disabled = !editable;
    }
  }
  for (const r of document.querySelectorAll(".ro")) {
    r.classList.toggle("hidden", editable);
    if (S.demo) r.textContent = T("Deze demo is alleen-lezen.");
  }
}
for (const form of document.querySelectorAll("form.controls")) {
  form.addEventListener("change", async (e) => {
    const el = e.target;
    const value = el.type === "checkbox" ? el.checked : el.tagName === "SELECT" ? JSON.parse(el.value) : Number(el.value);
    const r = await fetch("/api/settings", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ [el.name]: value }) });
    const msg = form.nextElementSibling;
    if (msg) msg.textContent = r.ok ? T("Opgeslagen.") : T("Niet opgeslagen:") + " " + await r.text();
  });
}

// ------------------------------------------------------------------ system / shadow
function cmdText(c) {
  if (typeof c === "string") return c;
  const [k, v] = Object.entries(c)[0];
  return k + " " + (typeof v === "object" ? JSON.stringify(v) : v);
}
function renderSystem(s) {
  fill("cmds", s.commands.slice(0, 15).map((c) => `<li><time>${time(c.at)}</time>${esc(cmdText(c.command))}${c.actuated ? "" : " <span class=note>" + T("(niet uitgevoerd)") + "</span>"}${c.error ? " <span class=pos>" + esc(c.error) + "</span>" : ""}</li>`).join("") || "<li class=note>" + T("nog geen") + "</li>");
  fill("notes", s.notifications.slice(0, 10).map((n) => `<li><time>${time(n.at)}</time><b>${esc(n.title)}</b> ${esc(n.message)}</li>`).join("") || "<li class=note>" + T("geen") + "</li>");
  const sh = s.shadow;
  $("shadow").classList.toggle("hidden", !sh);
  if (sh) {
    $("shf").textContent = sh.field_agreement_pct === null ? "–" : num(sh.field_agreement_pct, 1) + " %";
    $("shfs").textContent = T("sinds") + " " + time(sh.since) + " · " + sh.ticks + " ticks · " + sh.log_lines + " " + T("logregels");
    $("shc").textContent = sh.command_agreement_pct === null ? "–" : num(sh.command_agreement_pct, 1) + " %";
    $("shcs").textContent = sh.commands.reduce((a, k) => a + k.rust + k.ha, 0) + " " + T("acties vergeleken");
    $("shcmd").innerHTML = sh.commands.map((k) => `<tr><td>${esc(k.key)}</td><td class=n>${k.rust_matched}/${k.rust}</td><td class=n>${k.ha_matched}/${k.ha}</td><td class=n>${k.agreement_pct === null ? "–" : num(k.agreement_pct, 1)}</td><td>${k.last_mismatch ? time(k.last_mismatch.at) + " " + esc(k.last_mismatch.side) + " " + val(k.last_mismatch.val) : ""}</td></tr>`).join("");
    $("shfld").innerHTML = sh.fields.filter((f) => f.samples > 0).slice(0, 40).map((f) => `<tr><td title="${esc(f.entity)}">${esc(f.field)}</td><td class=n>${num(f.agreement_pct, 1)}</td><td class=n>${f.max_err === null ? "–" : val(f.max_err)}</td><td>${f.last_mismatch ? time(f.last_mismatch.at) + " " + val(f.last_mismatch.rust) + " / " + val(f.last_mismatch.ha) : ""}</td></tr>`).join("");
  }
}

// ------------------------------------------------------------------ control mode
const DEVNAME = { evse: "SmartEVSE (Buzz)", cable: "Laadkabel (EV3)", solis: "Solis", airco: "Airco's", prices: "Prijzen" };
const writeText = (w) => esc(w.target) + " = " + esc(JSON.stringify(w.value)) + (w.before === undefined ? "" : " <span class=note>(was " + esc(JSON.stringify(w.before)) + ")</span>");
async function postControl(body) {
  const r = await fetch("/api/control", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body) });
  $("ctlnote").textContent = r.ok ? "" : T("Niet gelukt:") + " " + await r.text();
}
function renderControl(s) {
  const c = s.control;
  $("control").classList.toggle("hidden", !c);
  $("ctl").classList.toggle("hidden", !c);
  if (!c) return;
  const state = T(c.dry_run ? "proefdraaien: er wordt niets geschreven" : c.enabled ? "sturing aan" : "sturing UIT");
  $("ctl").textContent = c.dry_run ? "dry run" : T(c.enabled ? "sturing aan" : "sturing uit");
  $("ctl").className = "badge " + (c.dry_run ? "" : c.enabled ? "ok" : "bad");
  $("ctlstate").textContent = state + " · " + [[c.stats.sent, "verstuurd"], [c.stats.verified, "bevestigd"], [c.stats.failed, "niet bevestigd"], [c.stats.blocked, "tegengehouden"], [c.stats.dry_run, "proef"]].map(([n, k]) => n + " " + T(k)).join(", ");
  $("ctlkill").textContent = T(c.enabled ? "Alles stoppen" : "Sturing weer aan");
  $("ctlkill").onclick = () => postControl({ enabled: !c.enabled });
  const form = $("ctldevs");
  form.innerHTML = Object.entries(c.devices).map(([d, on]) => `<label><input type=checkbox data-dev="${esc(d)}"${on ? " checked" : ""}> ${esc(T(DEVNAME[d] || d))}</label>`).join(" ");
  form.onchange = (e) => postControl({ device: e.target.dataset.dev, enabled: e.target.checked });
  $("ctlnote").textContent = "Log: " + c.log_file;
  $("ctlrows").innerHTML = c.recent.slice(0, 25).map((a) => `<tr><td>${time(a.at)}</td><td>${esc(a.device)}</td><td>${esc(cmdText(a.command))}</td><td>${a.writes.map(writeText).join("<br>") || "<span class=note>" + T("niets") + "</span>"}</td><td>${a.field ? esc(a.field) + ": " + val(a.before) + " → " + val(a.after) : ""}</td><td${a.result === "failed" || a.result === "error" ? " class=pos" : ""}>${esc(a.result)}${a.detail ? " " + esc(a.detail) : ""}</td></tr>`).join("") || "<tr><td colspan=6 class=note>" + T("nog geen") + "</td></tr>";
}

// Every device instance's entities (groups other than the logic's own).
const LOGIC = new Set(["world", "derived", "setting", "control"]);
function renderDevices() {
  const rows = entities.filter((e) => !LOGIC.has(e.group));
  $("devices").classList.toggle("hidden", rows.length === 0);
  $("devrows").innerHTML = rows.map((e) => `<tr><td>${esc(e.group)}</td><td>${esc(T(e.name))}</td><td class=n>${val(e.value)}${e.unit ? " " + esc(e.unit) : ""}</td></tr>`).join("");
}
function applyEntities(changed) {
  if (!entities.length || !changed || !Object.keys(changed).length) return;
  for (const ent of entities) if (ent.id in changed) ent.value = changed[ent.id];
  renderDevices();
}

// ------------------------------------------------------------------ main
function render(s) {
  S.snap = s;
  if (s.now) S.now = Date.parse(s.now);
  $("now").textContent = s.now ? new Date(s.now).toLocaleString(LOCALE) + " · tick " + s.tick : T("wacht op eerste tick…");
  const ha = $("ha");
  if (s.ha) { ha.textContent = T(s.ha.connected ? "HA verbonden" : "HA niet verbonden"); ha.className = "badge " + (s.ha.connected ? "ok" : "bad"); }
  else { ha.textContent = T("zonder HA"); ha.className = "badge"; }
  renderTiles(s);
  renderFlow(s);
  renderSystem(s);
  renderControl(s);
  syncControls(s.settings, s.settings_editable && !S.demo);
  syncEntityControls();
}

I18N.apply(document.body);
I18N.button($("lang"));
render(JSON.parse($("initial").textContent));
showTab(location.hash.slice(1) || "overzicht");
loadPrices().then(() => render(S.snap));
loadHistMeta();
fetch("/api/entities").then((r) => r.json()).then((list) => { entities = list; renderDevices(); render(S.snap); }).catch(() => {});
setInterval(() => { refreshCharts(false); }, 5000);
setInterval(() => { loadPrices(); loadHistMeta(); }, 60000);
// A public demo says so and keeps its controls off. The server refuses writes either way.
fetch("/api/demo").then((r) => {
  if (!r.ok) return;
  S.demo = true;
  $("demotext").textContent = T("Demo: een gesimuleerd huis waarin de tijd sneller loopt. Niets hier is echt en de bediening staat uit.");
  $("demolink").textContent = T("Broncode");
  $("demo").classList.remove("hidden");
  render(S.snap);
}).catch(() => {});
// With a sign-in session: who is signed in, and a way out.
fetch("/auth/me").then((r) => (r.ok ? r.json() : null)).then((me) => {
  if (!me || !me.logout) return;
  const form = document.createElement("form");
  form.method = "post";
  form.action = "/auth/logout";
  form.style.display = "inline";
  const button = document.createElement("button");
  button.textContent = T("Afmelden") + " (" + me.user + ")";
  form.append(" · ", button);
  $("signout").append(form);
}).catch(() => {});
const es = new EventSource("/api/events");
es.addEventListener("state", (e) => {
  const s = JSON.parse(e.data);
  applyEntities(s.entities);
  render(s);
});
es.onerror = () => {
  // The session has ended: back through the sign-in.
  fetch("/api/entities", { method: "HEAD" }).then((r) => { if (r.status === 401) location.reload(); }).catch(() => {});
  $("ha").textContent = T("verbinding met energie verbroken"); $("ha").className = "badge bad"; };
