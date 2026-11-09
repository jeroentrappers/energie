// The dashboard's languages. The Dutch text is the source and the key; EN maps it to English and
// HTML holds the longer passages, marked `data-i18n` in the page. The choice comes from the
// browser, or from the language button once that has been used.
"use strict";
const I18N = (() => {
  const EN = {
    // Tabs and sections
    "Overzicht": "Overview", "Capaciteitstarief": "Capacity tariff", "Prijzen": "Prices", "Batterij & Solis": "Battery & Solis",
    "EV laden": "EV charging", "Systeem": "System", "Energiestroom": "Power flow", "🎯 Laaddoelen": "🎯 Charging targets", "Stromen": "Flows",
    "Grafiekperiode": "Chart period", "Voorspellingsperiode": "Forecast period", "Bediening": "Controls", "Batterij": "Battery",
    "Net & fasebelasting — 3×32 A": "Grid & phase load — 3×32 A", "Zonnepanelen": "Solar panels", "EV's": "EVs", "Dagbalans (vandaag)": "Daily balance (today)",
    "Nacht & overig": "Night & other", "Aircos": "Air conditioners", "Airco beneden": "Downstairs AC", "Airco zolder": "Attic AC",
    "Kwartierbudget (live)": "Quarter-hour budget (live)", "Dit kwartier: gemiddelde vs target": "This quarter: average vs target",
    "Laatste 12 u — % van target per kwartier": "Last 12 h — % of target per quarter", "Deze maand": "This month", "Piekhistoriek (12 maanden)": "Peak history (12 months)",
    "Maand": "Month", "Piek": "Peak", "Telt mee t.e.m.": "Counts through", "Hoe lees je dit?": "How to read this", "Verloop": "History",
    "Kwartiervermogen — laatste 24 u": "Quarter-hour power — last 24 h", "Dagpieken (14 d) — hoogste kwartier per dag": "Daily peaks (14 d) — highest quarter of each day",
    "Fluvius-controle (officiële meterdata)": "Fluvius check (official meter data)", "Maandpieken — Fluvius vs lokaal (12 maanden)": "Monthly peaks — Fluvius vs local (12 months)",
    "Lokaal (P1)": "Local (P1)", "Verschil": "Difference", "Piekkwartier": "Peak quarter", "Day-ahead curve": "Day-ahead curve",
    "Ecopower kwartierprijzen — vandaag & morgen": "Ecopower quarter-hour prices — today & tomorrow", "Nu en vandaag": "Now and today",
    "Vaste kosten (niet in kWh-prijs)": "Fixed costs (not in the kWh price)", "Geldverbruik per kwartier": "Cost per quarter hour",
    "Geldverbruik per kwartier (verbruik × prijs)": "Cost per quarter hour (consumption × price)", "Energiekost per dag (laatste 14 d)": "Energy cost per day (last 14 d)",
    "Toelichting": "Explanation", "Status": "Status", "Besturing (omvormer, alleen-lezen)": "Control (inverter, read-only)",
    "Batterij — vermogen & SOC (24 u)": "Battery — power & SOC (24 h)", "PV-productie (24 u)": "PV production (24 h)", "⚙️ Laadsturing": "⚙️ Charging control",
    "Modus & prioriteit": "Mode & priority", "Slimme laadkabel (1f, 6–16 A)": "Smart charging cable (1-phase, 6–16 A)", "SmartEVSE (1f, tot 32 A)": "SmartEVSE (1-phase, up to 32 A)",
    "Laadkosten & vergelijking": "Charging costs & comparison", "Sturing": "Control", "Alles stoppen": "Stop everything", "Sturing weer aan": "Turn control back on",
    "Tijd": "Time", "Apparaat": "Device", "Commando": "Command", "Schrijft": "Writes", "Terug gelezen": "Read back", "Resultaat": "Result",
    "Commando's": "Commands", "Meldingen": "Notifications", "Bronnen & opslag": "Sources & storage", "Apparaten": "Devices", "Waarde": "Value", "Nu": "Now",
    "Shadow — overeenkomst met Home Assistant": "Shadow — agreement with Home Assistant", "Sensoren": "Sensors", "Acties": "Actions", "Sleutel": "Key",
    "Laatste verschil": "Last difference", "Sensoren (slechtste eerst)": "Sensors (worst first)", "Veld": "Field", "Max fout": "Max error",
    "Laatste verschil (Rust / HA)": "Last difference (Rust / HA)", "sturing": "control",
    // Chart periods and titles
    "Laatste 6 uur": "Last 6 hours", "Laatste 12 uur": "Last 12 hours", "Laatste 24 uur": "Last 24 hours", "Gisteren": "Yesterday", "Laatste 7 dagen": "Last 7 days",
    "Vandaag": "Today", "Vandaag + morgen": "Today + tomorrow", "Nu ±12u": "Now ±12 h", "laatste 6 uur": "last 6 hours", "laatste 12 uur": "last 12 hours",
    "laatste 24 uur": "last 24 hours", "gisteren": "yesterday", "7 dagen": "7 days", "vandaag + morgen": "today + tomorrow", "nu ±12u": "now ±12 h", "vandaag": "today",
    "Zon & prijs": "Solar & price", "Energiestroom — laatste 6 uur": "Power flow — last 6 hours", "Zon & prijs — vandaag": "Solar & price — today",
    // Tiles and pills
    "Prijs nu": "Price now", "Niveau": "Level", "Volgende sturing": "Next control update", "Kwartier": "Quarter", "Maandpiek": "Monthly peak", "PIEKRISICO": "PEAK RISK",
    "Projectie": "Projection", "Vastgelegd": "Committed", "Net nu": "Grid now", "Beschikbaar": "Available", "Modus": "Mode", "Prijs": "Price",
    "Net (P1)": "Grid (P1)", "Zon": "Solar", "Huis": "Home", "afname": "import", "injectie": "export", "ontlaadt": "discharging", "laadt": "charging", "rust": "idle",
    "Kia EV3 (laadkabel)": "Kia EV3 (charging cable)", "Doel (batt + P1)": "Target (batt + P1)", "Werkelijk": "Actual", "Sturing bezig": "Control active",
    "Volgende update": "Next update", "Geladen vandaag": "Charged today", "Ontladen vandaag": "Discharged today", "Net (W)": "Grid (W)", "PV-overschot netto": "Net PV surplus",
    "PV-overschot bruto": "Gross PV surplus", "Maandpiek-doel": "Monthly peak target", "Piekrisico": "Peak risk", "Guard-interventies": "Guard interventions",
    "Totaal nu": "Total now", "Forecast nu": "Forecast now", "geen forecastbron": "no forecast source", "PV vandaag (Solis + Omnik)": "PV today (Solis + Omnik)",
    "EV3 kabel": "EV3 cable", "EV3 stroom": "EV3 current", "EV3 accu": "EV3 battery", "Buzz stekker": "Buzz plug", "Buzz status": "Buzz status", "Buzz accu": "Buzz battery",
    "EV3 AC-limiet": "EV3 AC limit", "EV3 kilometerstand": "EV3 odometer", "Buzz laaddoel": "Buzz charge target", "Buzz kilometerstand": "Buzz odometer",
    "Dump bezig": "Dump active", "Zelfverbruik (van PV)": "Self-consumption (of PV)", "Zelfvoorziening (van verbruik)": "Self-sufficiency (of consumption)",
    "Verbruik": "Consumption", "PV productie": "PV production", "PV zelf gebruikt": "PV used on site", "Injectie": "Export", "Netimport": "Grid import",
    "Batterij geladen": "Battery charged", "Batterij ontladen": "Battery discharged", "Nachtdekking": "Night coverage", "Duur donkere periode": "Length of dark period",
    "Nacht uit batterij": "Night from battery", "Nacht uit net": "Night from grid", "Vriezer": "Freezer", "Mag nu": "Allowed now", "Verbruikt dit kwartier": "Used this quarter",
    "Maandpiek (Fluvius)": "Monthly peak (Fluvius)", "Maandpiek × tarief": "Monthly peak × tariff", "Gefactureerde piek (12 mnd gem.)": "Billed peak (12-month avg)",
    "Op factuur deze maand": "On this month's bill", "Doel rest van maand": "Target for rest of month", "Projectie lopend kwartier": "Projection for this quarter",
    "Vastgelegd lopend kwartier": "Committed this quarter", "Import dit kwartier": "Import this quarter", "Maandpiek Fluvius": "Fluvius monthly peak",
    "Maandpiek lokaal (P1)": "Local monthly peak (P1)", "Afname dit kwartier (all-in)": "Import this quarter (all-in)", "Volgend kwartier": "Next quarter",
    "Injectie dit kwartier": "Export this quarter", "Positie in dagrange": "Position in day's range", "Laagste kwartier": "Lowest quarter", "Tijd laagste": "Time of lowest",
    "Hoogste kwartier": "Highest quarter", "Tijd hoogste": "Time of highest", "Abonnement + databeheer + energiefonds": "Subscription + data management + energy fund",
    "per maand": "per month", "Capaciteitstarief (maandpiek × tarief)": "Capacity tariff (monthly peak × tariff)", "Capaciteitstarief op factuur (12 mnd gem.)": "Capacity tariff on the bill (12-month avg)",
    "Kost-snelheid nu (€/u)": "Cost rate now (€/h)", "Energiekost vandaag": "Energy cost today", "Beschikbare energie": "Available energy", "Vermogen (− = laden)": "Power (− = charging)",
    "Doel (setpoint)": "Target (setpoint)", "Vandaag geladen": "Charged today", "Vandaag ontladen": "Discharged today", "Batterij → auto geleverd": "Battery → car delivered",
    "Directe sturing": "Direct control", "Laadvermogen (RC)": "Charge power (RC)", "Ontlaadvermogen (RC)": "Discharge power (RC)", "Time-out (RC)": "Timeout (RC)",
    "Netladen toegestaan": "Grid charging allowed", "Injectielimiet": "Export limit", "Solis offline": "Solis offline", "Dispatcher off-target": "Dispatcher off target",
    "Stekker": "Plug", "verbonden": "connected", "los": "unplugged", "AC-limiet": "AC limit", "Kilometerstand": "Odometer", "Laadvermogen": "Charging power", "Laadt": "Charging",
    "DC-limiet": "DC limit", "Laatste update": "Last update", "Doel-SOC": "Target SOC", "Kabel": "Cable", "Laatste voertuigupdate": "Last vehicle update", "Laadstroom": "Charge current",
    "Vermogen": "Power", "Stroom": "Current", "Spanning": "Voltage", "Override stroom": "Override current", "Laadvermogen (→ Buzz)": "Charging power (→ Buzz)", "Laadstroom (pilot)": "Charge current (pilot)",
    "Auto verbonden": "Car connected", "Actieve laadauto": "Active car", "EV-laadvermogen zon": "EV charging from solar", "EV-laadvermogen net": "EV charging from grid", "Benzineprijs": "Petrol price",
    "Zon geladen (kWh)": "Charged from solar (kWh)", "Net geladen (kWh)": "Charged from grid (kWh)", "Zon-aandeel": "Solar share", "Netkost totaal": "Total grid cost", "Netkost deze maand": "Grid cost this month",
    "Kost/100km (EV)": "Cost/100 km (EV)", "Kost/100km (ICE)": "Cost/100 km (petrol)", "Besparing/100km": "Saving/100 km", "Verbruik/100km": "Consumption/100 km", "Kost-snelheid nu": "Cost rate now",
    "Bronnen": "Sources", "Actuatoren": "Actuators", "Statusbestanden": "State files", "Historiek": "History", "Historiek geschreven": "History written", "uit": "off", "aan": "on",
    "ja": "yes", "nee": "no", "niet verbonden": "not connected", "niet gebruikt": "not used", "aanmelden": "sign-in needed", "onbereikbaar": "unreachable", "leeg": "empty",
    "zelfverbruik": "self-consumption", "geforceerd laden": "forced charging", "geforceerd ontladen": "forced discharging", "goedkoop": "cheap", "normaal": "normal", "duur": "expensive",
    // Fragments used in composed text
    "basis": "base", "laden": "charging", "ontladen": "discharging", "vasthouden": "hold", "doel": "target", "doel laden": "target charging", "doel ontladen": "target discharging",
    "doel vasthouden": "target hold", "sinds start": "since start", "sinds": "since", "zon": "solar", "net": "grid", "maand": "month", "/u": "/h", "entiteiten": "entities",
    "berichten": "messages", "keer geschreven": "writes", "metingen": "readings", "rijen": "rows", "fouten": "errors", "lopend": "running", "airco": "AC", "beneden": "downstairs", "zolder": "attic",
    "logregels": "log lines", "acties vergeleken": "actions compared",
    // Flow chart and series
    "Net": "Grid", "Batterij (+ontl/−laad)": "Battery (+disch/−chg)", "Net (+afn/−inj)": "Grid (+imp/−exp)", "Huis (basis)": "Home (base)", "PV werkelijk": "PV actual", "Afname": "Import",
    "Gemiddelde dit kwartier": "Average this quarter", "Actueel (afname)": "Actual (import)", "Target (Fluvius)": "Target (Fluvius)", "Kwartier behaald": "Quarter reached",
    "Kwartiergemiddelde": "Quarter average", "Dagpiek": "Daily peak", "Afname (all-in)": "Import (all-in)", "€ per kwartier": "€ per quarter", "Afname all-in": "Import all-in",
    "Netto kost/dag": "Net cost/day", "% van target": "% of target", "nu": "now",
    // Controls
    "Zonprioriteit": "Solar priority", "Net (grid)": "Grid", "Uit": "Off", "6A-brug": "6 A bridge", "Max tot piek": "Max to peak", "Ontlaad-budget": "Discharge budget",
    "Batterij-dump budget": "Battery dump budget", "EV-laden aan": "EV charging on", "Batterijsturing": "Battery control", "Airco-dump": "AC dump", "Batterij-reservering": "Battery reservation",
    "Reservering": "Reservation", "Batterij reserve (ontlaadvloer)": "Battery reserve (discharge floor)", "Capaciteitsdoel (optrekken)": "Capacity target (raise)",
    "Goedkoop laden: prijsdrempel": "Cheap charging: price limit", "Airco-dump: overschot-drempel": "AC dump: surplus threshold", "Airco-dump: SoC-drempel": "AC dump: SoC threshold",
    "EV3 vergelijkbaar verbruik": "EV3 comparable consumption", "Buzz vergelijkbaar verbruik": "Buzz comparable consumption", "EV3 km-stand bij start": "EV3 odometer at start",
    "Buzz km-stand bij start": "Buzz odometer at start", "Batterij → auto: geleverd": "Battery → car: delivered", "→ actief maandtarget": "→ active monthly target",
    "→ meerkost piek (€/maand)": "→ extra peak cost (€/month)", "→ effectieve reservering": "→ effective reservation", "Instelbare drempels": "Adjustable thresholds", "Auto's (vergelijking)": "Cars (comparison)",
    "AC-laadlimiet instellen": "Set AC charging limit", "Alleen-lezen: in shadow-lockstep komen de instellingen uit Home Assistant.": "Read-only: in shadow lockstep the settings come from Home Assistant.",
    // Internal values on the System tab
    "Batterij setpoint-bias": "Battery setpoint bias", "Batterij laden van het net": "Battery charging from grid", "Batterijsturing bezig": "Battery control active",
    "Airco-dump bezig": "AC dump active", "Batterij-dump startmeting": "Battery dump start reading", "Hoger gerangschikte auto laadt niet": "Higher-ranked car not charging",
    "Laadstilstand sinds": "Charging stalled since", "Nacht EV-net startmeting": "Night EV grid start reading", "Nacht batterij": "Night battery",
    "Nacht import startmeting": "Night import start reading", "Nacht ontlading startmeting": "Night discharge start reading", "Nacht import": "Night import", "Nachtduur": "Night length",
    "Piekhistoriek": "Peak history", "Watchdog laatste herstart": "Watchdog last restart", "Peak guard interventies": "Peak guard interventions", "Buzz laadweigeringen": "Buzz charge refusals",
    "Volgende sturingscyclus": "Next control cycle", "Piekrisico gewijzigd": "Peak risk changed", "SmartEVSE modus laatst bekend": "SmartEVSE last known mode", "SmartEVSE stekker laatst bekend": "SmartEVSE last known plug",
    // Messages
    "Let op: de airco-dump en de piekwacht schakelen de aircos ook.": "Note: the AC dump and the peak guard switch the air conditioners too.",
    "Schakelen kan als een apparaat de aircos bestuurt (control = \"load:airco\", read_only = false) en niet in shadow-modus.": "Switching works when a device controls the air conditioners (control = \"load:airco\", read_only = false), and not in shadow mode.",
    " Er is nog geen PV-forecastbron: de forecastlijn verschijnt zodra er een is.": " There is no PV forecast source yet: the forecast line appears once there is one.",
    "Demo: een gesimuleerd huis waarin de tijd sneller loopt. Niets hier is echt en de bediening staat uit.": "Demo: a simulated house where time runs faster. Nothing here is real and the controls are off.",
    "Deze demo is alleen-lezen.": "This demo is read-only.", "Broncode": "Source code", "Sleep over de grafiek om de uren te kiezen waarin mag geladen worden.": "Drag across the chart to choose the hours when charging is allowed.", "Laadlimiet": "Charging limit",
    "Toepassen": "Apply", "Annuleren": "Cancel", "Verstuurd.": "Sent.", "Niet verstuurd:": "Not sent:", "Opgeslagen.": "Saved.", "Niet opgeslagen:": "Not saved:", "Niet gelukt:": "Failed:", "Geen historiek:": "No history:",
    "(niet uitgevoerd)": "(not carried out)", "nog geen": "none yet", "geen": "none", "niets": "nothing", "wacht op eerste tick…": "waiting for the first tick…",
    "HA verbonden": "HA connected", "HA niet verbonden": "HA not connected", "zonder HA": "without HA", "verbinding met energie verbroken": "connection to energie lost", "Afmelden": "Sign out",
    "proefdraaien: er wordt niets geschreven": "dry run: nothing is written", "sturing aan": "control on", "sturing UIT": "control OFF", "sturing uit": "control off", "verstuurd": "sent",
    "bevestigd": "confirmed", "niet bevestigd": "not confirmed", "tegengehouden": "blocked", "proef": "dry run", "SmartEVSE (Buzz)": "SmartEVSE (Buzz)", "Laadkabel (EV3)": "Charging cable (EV3)", "Airco's": "Air conditioners",
  };
  const HTML = {
    fc_note: 'Outlook: <b>PV forecast</b> vs actual production on the kW axis, <b>day-ahead prices</b> (import/export) on the €/kWh axis. Tomorrow appears once the day-ahead prices are published (around 14:00).<span id="fcnote"></span>',
    cap_how: `<p><b>Capacity tariff (Fluvius):</b> the monthly peak is the highest quarter-hour average of net import (minimum <b>2.5 kW</b>). You pay on the <b>average of the last 12 monthly peaks</b> (<i>Billed peak</i>), not on this month's peak alone.</p>
      <ul><li><b>Committed</b> = what this quarter has already reached (energy so far × 4).</li><li><b>Projection</b> = committed + current power for the rest of the quarter.</li></ul>
      <p>Halle-Vilvoorde tariff 2026: <b>€ 4.95/kW/month</b> (incl. VAT), minimum 2.5 kW = € 12.38/month. Each kW of monthly peak above 2.5 costs about € 0.41/month extra, <b>for 12 months</b> (about € 4.95 in total). <i>Monthly peak × tariff</i> is what you would pay if all 12 months were as low as this one.</p>
      <p><b>Dynamic target:</b> the monthly peak is a high-water mark: once set, it can't come down that month. So <i>Target for rest of month</i> = max(2.5 kW, peak already set). If the peak is already above 2.5 (say an EV at 16 A), consumption up to that level is <b>free</b> for the rest of the month and the guard only steps in above it. On the 1st of the month everything resets to 2.5 kW.</p>
      <p>The red <i>PEAK RISK</i> badge appears just below the dynamic target: first the EV current is reduced, then the battery discharges, then the heat pump pauses.</p>`,
    peak_source: 'Source: <code>capaciteit_piekhistoriek</code> (Fluvius export, then extended every month with the P1 peak).',
    fluvius_note: '<b>Official Fluvius data.</b> The monthly peak confirms the local measurement, which is the basis of the capacity tariff. Note that Fluvius data runs <b>about 1 day behind</b> (the portal updates overnight).',
    price_how: `<p>All prices are <b>quarter-hour prices, all-in</b>, following the Ecopower tariff card (Jan 2026) + Fluvius Halle-Vilvoorde:</p>
      <p><b>Import</b> = (1.02 × EPEX DA + 0.004 [margin] + 0.011 [green certificates] + 0.00392 [CHP] + 0.0531770 [Halle-Vilvoorde offtake tariff] + 0.0019261 [energy contribution] + 0.04748 [special excise]) × 1.06 VAT: a fixed addition on top of spot of <b>about 0.129 €/kWh incl. VAT</b>.</p>
      <p><b>Export</b> = 0.98 × EPEX DA − 0.015 (no VAT). Not included: capacity tariff (per kW monthly peak, see the Capacity tariff tab), data management €17.85/yr, subscription €5/month.</p>
      <p>Battery arbitrage only pays when the price difference between charging and discharging is larger than the round-trip loss (about 15 %). Always compare import with import price (shifting) or export with import price (self-consumption).</p>`,
    batt_note: 'The battery control (dispatcher) writes these registers. Turn it on or off with <i>Battery control</i> under Controls.',
    ev_how: `<ul>
        <li><b>Grid</b> and <b>Battery</b>: what the EV does when the sun is <b>too weak</b> for the 6 A minimum. <i>Off</i> = pause · <i>6 A bridge</i> = top up to 6 A from grid or battery · <i>Max to peak</i> = grid up to just below the peak · <i>Discharge budget</i> = battery until the budget is used.</li>
        <li><b>Solar priority</b>: with a <b>surplus</b>, who gets the sun first: EV3, Buzz or battery?</li>
      </ul>
      <p><i>In short: levels = what to do with too little sun · solar priority = who gets too much sun.</i></p>`,
    cable_note: '🔌 The smart charging cable controls the <b>EV3</b>; the <b>SmartEVSE</b> controls the <b>Buzz</b>.',
    evse_note: '🔌 <b>SmartEVSE → always Buzz.</b> <i>Charging power</i> is single-phase and estimated from pilot current × 230 V (no CT measurement).',
  };

  const key = "energie.lang";
  const read = () => { try { return localStorage.getItem(key); } catch { return null; } };
  const lang = read() || (/^nl\b/i.test(navigator.language || "") ? "nl" : "en");
  const en = lang === "en";
  const locale = en ? "en-GB" : "nl-BE";
  const one = (s) => EN[s] ?? s;
  // The solar-priority options read "Buzz → Batterij → EV3".
  // Settings carry their unit: "Reservering (W)".
  const withUnit = (s) => { const m = /^(.+) (\([^()]+\))$/.exec(s); return m && EN[m[1]] ? EN[m[1]] + " " + m[2] : null; };
  const T = (s) => !en || typeof s !== "string" ? s : EN[s] ?? withUnit(s) ?? (s.includes(" → ") && !s.startsWith("→") ? s.split(" → ").map(one).join(" → ") : s);

  function apply(root) {
    document.documentElement.lang = lang;
    if (!en) return;
    for (const e of root.querySelectorAll("[data-i18n]")) if (HTML[e.dataset.i18n]) e.innerHTML = HTML[e.dataset.i18n];
    const walk = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
    for (let n; (n = walk.nextNode());) {
      if (n.parentElement.closest("script,style,code,[data-i18n]")) continue;
      const s = n.nodeValue.trim();
      if (!s) continue;
      const out = T(s);
      if (out !== s) n.nodeValue = n.nodeValue.replace(s, out);
    }
  }

  function button(el) {
    el.textContent = en ? "NL" : "EN";
    el.title = en ? "Nederlands" : "English";
    el.addEventListener("click", () => {
      try { localStorage.setItem(key, en ? "nl" : "en"); } catch { /* private mode */ }
      location.reload();
    });
  }

  return { T, en, locale, apply, button };
})();
