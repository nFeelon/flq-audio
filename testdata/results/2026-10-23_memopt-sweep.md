# 2026-10-23 — corpus sweep on memory-optimization build

`flq bench` over `sample_pack/` (lists `a`+`b`, 256 entries), build
`564754b` + uncommitted memory-optimization diff, `-5` defaults, all
cores. 172 files benched; the other 84 entries are mp3/ogg/m4a (v1 is
lossless-only — rejected by design) plus `750548_simulated-digital-data.flac`,
a truncated FLAC: `demuxer: unexpected end of file` (pre-existing hard
error, unchanged by this work). All 172 benched tracks `audio-exact`.

Totals: 1,817,593,975 B in → 1,067,755,601 B out = **58.75%**
(wav-only subset 52.7%; flac rewrap 97.8%; aiff 77.7%).
enc 154.6 s total, dec 33.2 s total (two sweep halves ran concurrently —
wall times inflated by CPU contention). Max peak heap: enc 141.1 MiB,
dec 137.6 MiB (both on the 75 MiB `178465…royal-air-force-radio-04.flac`;
`bench` holds the in-memory .flq image by design).

vs the prior `single.csv` (pre-v1 codec baseline — directional only):
all common files smaller; max enc heap 1603.9→141.1 MiB, max dec
2494.6→137.6 MiB; median enc 58.2→18.9, median dec 56.2→14.1 MiB.
Worse by >1 MiB: 2 enc (≤17.0), 3 dec (≤9.8) — small files at the
thread/window floor.

Repro (outside repo): `D:\TEMP\flq_sweep\sweep.ps1` + `parse.py` →
`single.csv`; diff script `diff_run.py`.

## Per-file (in → out bytes, %, exactness)
| file | in (B) | out (B) | % | exact |
|---|---|---|---|---|
| 117635__timbre__remix-of-117609__klankbeeld__60_cycle_hum_request_for_user_bryschnei.flac | 849701 | 553408 | 65.13 | audio-exact |
| 120480__timbre__video-warble-bleep-artifact.flac | 82078 | 57540 | 70.10 | audio-exact |
| 122288__timbre__alien-radio-interference.flac | 10807119 | 10719863 | 99.19 | audio-exact |
| 128472__klankbeeld__royal-air-force-radio-03.flac | 45621568 | 44794355 | 98.19 | audio-exact |
| 129217__klankbeeld__all-units-proceed-to-your-post-assignments.wav | 172315 | 130933 | 75.98 | audio-exact |
| 173510__timbre__ultrasonic-video-soundtrack-artifact-slowed_2.flac | 85241 | 42173 | 49.48 | audio-exact |
| 178465__klankbeeld__royal-air-force-radio-04.flac | 75416764 | 75065385 | 99.53 | audio-exact |
| 210894__timbre__processed-mp3-artifact-bell-ish.wav | 238372 | 54719 | 22.96 | audio-exact |
| 210895__timbre__processed-mp3-artifacts-2.wav | 675468 | 318295 | 47.12 | audio-exact |
| 210898__timbre__tracking-randomess.flac | 3284466 | 3151068 | 95.94 | audio-exact |
| 210899__timbre__clangers-on-speed.flac | 3807200 | 2922067 | 76.75 | audio-exact |
| 240770__klankbeeld__royal-air-force-radio-04.wav | 55188875 | 32516248 | 58.92 | audio-exact |
| 243072__klankbeeld__frequency-modulation-coding.wav | 1157783 | 913397 | 78.89 | audio-exact |
| 243351__klankbeeld__websdr_recording_2014-07-21t07_22_13z_70114khz.wav | 7577240 | 4329130 | 57.13 | audio-exact |
| 248088__timbre__subject-correct-remix-of-112295__timbre__love-you-to-bits-ansaphone-message-a-law.flac | 108650 | 106080 | 97.63 | audio-exact |
| 249782__timbre__star-trek-medical-scanner-ish.flac | 188306 | 110891 | 58.89 | audio-exact |
| 263674__klankbeeld__dutch-radio-amteurs-nl.wav | 33493292 | 24091752 | 71.93 | audio-exact |
| 343491__klankbeeld__tug-boat-radio-nl.wav | 51893036 | 26497218 | 51.06 | audio-exact |
| 370328__klankbeeld__crazy-radio-signal-140727khz.wav | 2906668 | 1855354 | 63.83 | audio-exact |
| 397798__klankbeeld__raf-volmet-170710.wav | 8405142 | 5365811 | 63.84 | audio-exact |
| 398610__klankbeeld__weak-ham-radio-14237khz-170726.wav | 8794534 | 5997174 | 68.19 | audio-exact |
| 410387__klankbeeld__ham-repeater-uk-3cm-171030-10368850khz.wav | 7801388 | 4286507 | 54.95 | audio-exact |
| 411558__klankbeeld__airport-amsterdam-nl.wav | 10544499 | 3664223 | 34.75 | audio-exact |
| 414452__klankbeeld__strange-radio-signal-2017-11-10t22_30_14z_14575-khz.wav | 12812129 | 2597881 | 20.28 | audio-exact |
| 416608__klankbeeld__crazy-radio-websdr_recording_2017-12-31t13_26_56z_17300khz.wav | 1327148 | 1052462 | 79.30 | audio-exact |
| 441690__timbre__on-screen-display-osd-text-digital-noise.wav | 1087226 | 680066 | 62.55 | audio-exact |
| 499692__timbre__skewed-version-of-veilers-freesound-497914.wav | 314902 | 33576 | 10.66 | audio-exact |
| 528810__newlocknew__carpet-nord-winddr-05x3lrsgrnlzrfltr_frkfltreq.wav | 10434326 | 6386400 | 61.21 | audio-exact |
| 529780__newlocknew__forestwindytrees-noisepines-creakeqmsprcssng.wav | 46412360 | 36420154 | 78.47 | audio-exact |
| 534169__klankbeeld__ham-radio-italia-received-nl-170801.wav | 6061898 | 4106039 | 67.74 | audio-exact |
| 536286__timbre__artifactual-remix-of-xhale303s-freesound-535976.flac | 5888937 | 5836707 | 99.11 | audio-exact |
| 554164__klankbeeld__morse-on-ham-radio-170805.wav | 5759532 | 3971168 | 68.95 | audio-exact |
| 554506__klankbeeld__raf-volmet-170710.wav | 2940034 | 1936256 | 65.86 | audio-exact |
| 610965__timbre__de-reverbed-remix-of-jmoore809s-freesound-610494.flac | 750542 | 709654 | 94.55 | audio-exact |
| 611255__newlocknew__windsynth-5very-fierce-winter3osc.wav | 15894276 | 9128748 | 57.43 | audio-exact |
| 611256__newlocknew__windsynth-4very-fierce-winter3osc.wav | 7895574 | 4065677 | 51.49 | audio-exact |
| 611257__newlocknew__windsynth-3winter3osc.wav | 8803510 | 4667003 | 53.01 | audio-exact |
| 611258__newlocknew__draft-and-windsynth-12osc.wav | 7740438 | 2095658 | 27.07 | audio-exact |
| 612184__newlocknew__windsynth-52very-fierce-wintermuffled2osc.wav | 15507620 | 11146440 | 71.88 | audio-exact |
| 612185__newlocknew__draft-and-windsynth-2indoorresonant3osc.wav | 15702696 | 3837108 | 24.44 | audio-exact |
| 614665__klankbeeld__weak-am-radio-channel-747-khz.wav | 971308 | 887051 | 91.33 | audio-exact |
| 614719__klankbeeld__weather-station-on-5450-khz-in-usb-raf-volmet-220113.wav | 5814254 | 4675436 | 80.41 | audio-exact |
| 624727__klankbeeld__thank-the-lord-radio-us-2020-02-13t12_07_51z_60700khz.wav | 3889196 | 2734533 | 70.31 | audio-exact |
| 626113__klankbeeld__citizens-band-nl-d-210522-27405khz.wav | 5906476 | 3481809 | 58.95 | audio-exact |
| 626117__klankbeeld__am-radio-weak-bonaire-2021-08-07.wav | 1310764 | 975633 | 74.43 | audio-exact |
| 626118__klankbeeld__chu-call-sign-shortwave-time-signal-radio-station-_333000-caribean.wav | 852012 | 623215 | 73.15 | audio-exact |
| 626119__klankbeeld__ham-db0hrf-taunus-relay-gruppe.wav | 2373952 | 1062566 | 44.76 | audio-exact |
| 626268__klankbeeld__flight-radio-netherlands-220123.wav | 22255074 | 14735336 | 66.21 | audio-exact |
| 646830__newlocknew__windvege_rustle-of-apple-trees-in-the-gardennightwindyroad-noise-1_em.wav | 22528316 | 11832059 | 52.52 | audio-exact |
| 654481__timbre__90-percent-dropped-codec.flac | 484051 | 446096 | 92.16 | audio-exact |
| 656070__timbre__looped-glitched-excerpt2-of-erokias-freesound-655700.flac | 2810251 | 2741333 | 97.55 | audio-exact |
| 658870__newlocknew__windint_draft-through-the-door-sealsmall-lobbyroom-tone_em.wav | 11638498 | 3105570 | 26.68 | audio-exact |
| 669871__newlocknew__windvege_wind-shaking-leaves-on-a-treeautumnedge-of-the-forestroad-noise_em.wav | 19701338 | 8790751 | 44.62 | audio-exact |
| 690305__timbre__extra-extra-digital-excerpt-of-strangehorizons-freesound-690297.flac | 1403204 | 1407667 | 100.32 | audio-exact |
| 698986__klankbeeld__8-morse-channels-simultaneously-230805-21018.wav | 1781292 | 840959 | 47.21 | audio-exact |
| 703779__klankbeeld__230926-russian-radio-state-anthem-of-the-union-of-soviet-socialist-republics-7032.wav | 8348972 | 5879637 | 70.42 | audio-exact |
| 705345__newlocknew__windint_draft-through-the-gapentrance-door-to-the-apartment-2_em.wav | 22467382 | 5972840 | 26.58 | audio-exact |
| 712972__timbre__loopable-excerpt-of-strangehorizons-freesound-712885.flac | 1058550 | 1020126 | 96.37 | audio-exact |
| 714573__timbre__maimed-shifted-remix-of-strangehorizons-freesound-714087.wav | 4096982 | 2625125 | 64.07 | audio-exact |
| 717426__klankbeeld__tire-chat-vredestein-banden-2023-12-05-26985.wav | 4519468 | 2783454 | 61.59 | audio-exact |
| 729336__newlocknew__windint_muffled-eerie-windfrom-large-room-with-wood-trimless-reverb.wav | 27312712 | 2864095 | 10.49 | audio-exact |
| 734113__timbre__frankensteinian-remix-of-sm_ovs-freesound-733766.flac | 6011433 | 5981441 | 99.50 | audio-exact |
| 750540__timbre__simulated-usb-microphone-whine-1khz-harmonics.wav | 1704268 | 371439 | 21.79 | audio-exact |
| 751059__newlocknew__winddsgn_mixed-foresttrees-in-the-windartificial.wav | 24196452 | 19638685 | 81.16 | audio-exact |
| 751981__newlocknew__windvege_big-meadowwindydistant-noise-of-the-forest.wav | 17314702 | 8704047 | 50.27 | audio-exact |
| 753470__timbre__bassndrum-stems-from-timbres-freesound-753343.flac | 1642957 | 1557191 | 94.78 | audio-exact |
| 757615__newlocknew__winddsgn_foresttrees-in-the-windmixed-forest.wav | 11903328 | 6455036 | 54.23 | audio-exact |
| 760434__newlocknew__wind_autumneveningresidential-areawind-in-the-trees-fencepassersby.wav | 41175874 | 22111822 | 53.70 | audio-exact |
| 760726__newlocknew__winddsgn_mountain-wind-with-gusts-and-drafts.wav | 17846552 | 4346739 | 24.36 | audio-exact |
| 773646__timbre__ddd-remix-of-danilov_roman_junes-freesound-773425.flac | 804885 | 774199 | 96.19 | audio-exact |
| 773905__newlocknew__winddsgn_the-wind-in-the-steppe-or-prairiewith-gusts-and-whistles.wav | 27726694 | 7990864 | 28.82 | audio-exact |
| 782297__newlocknew__wind_wintereveningresidential-areawind-in-the-treesbushesfence.wav | 54007938 | 26062897 | 48.26 | audio-exact |
| 786004__newlocknew__wind_wintermidnightresidential-areawind-in-the-treescrackingdistant-gusts.wav | 31271372 | 15918560 | 50.90 | audio-exact |
| 830287__newlocknew__wind_autumneveningresidential-areawind-in-the-treesbushesfencevarious-occasional-sounds.wav | 51820822 | 31317631 | 60.43 | audio-exact |
| 842537__klankbeeld__dutch-groningen-cb-radio-band-1123-am-260110-11_23_17z_27124.wav | 11010092 | 9605174 | 87.24 | audio-exact |
| 843839__klankbeeld__citizen-band-einhoven-netherlands-260120t16_48_pm.wav | 11468844 | 7563149 | 65.95 | audio-exact |
| 850414__newlocknew__wind_winterevening-in-the-yardwind-in-the-treesbushesfencegusts.wav | 32676220 | 17479533 | 53.49 | audio-exact |
| 857341__smkoski02__markerclick_stabilo.aiff | 18051190 | 13982366 | 77.46 | audio-exact |
| 857343__smkoski02__panclick_typeb.aiff | 21777014 | 17095959 | 78.50 | audio-exact |
| 857344__smkoski02__penclick_typea.aiff | 19130318 | 14999401 | 78.41 | audio-exact |
| 857345__smkoski02__penclick_typec.aiff | 22118006 | 17268417 | 78.07 | audio-exact |
| 857346__smkoski02__pills_medicine.aiff | 18720118 | 14529888 | 77.62 | audio-exact |
| 857347__smkoski02__pills_medicine.aiff | 37916822 | 29063689 | 76.65 | audio-exact |
| 864029__cat-fox_alex__sci-fi-explodsion-design-raw-material-1.flac | 101885 | 50666 | 49.73 | audio-exact |
| 866519__cat-fox_alex__sci-fi-sub-drop-1.flac | 408061 | 360697 | 88.39 | audio-exact |
| 867002__cat-fox_alex__granular-ambience-1_bass-pad.wav | 3549348 | 1585817 | 44.68 | audio-exact |
| 868635__cat-fox_alex__sci-fi-rumble-riser.wav | 379692 | 273054 | 71.91 | audio-exact |
| 868637__timbre__wraparound-sine-sweep.flac | 146064 | 127544 | 87.32 | audio-exact |
| 868877__cat-fox_alex__cartoon-fx-kings-arrive.wav | 1143564 | 537159 | 46.97 | audio-exact |
| 868898__smkoski02__fridge_opening_closing_sounds.wav | 9854750 | 4537411 | 46.04 | audio-exact |
| 868900__smkoski02__straw_drinking_plasticcup.wav | 16670206 | 10492432 | 62.94 | audio-exact |
| 869011__cat-fox_alex__minion-scream.wav | 474188 | 321546 | 67.81 | audio-exact |
| 869130__cat-fox_alex__random-rumble-design-20269.wav | 8317692 | 4080227 | 49.05 | audio-exact |
| 869131__cat-fox_alex__random-rumble-design_20269.wav | 6427692 | 3077734 | 47.88 | audio-exact |
| 869244__cat-fox_alex__random-rumble-design-20269.wav | 7802668 | 4835158 | 61.97 | audio-exact |
| 869245__cat-fox_alex__random-rumble-design-20269.wav | 29310868 | 18953667 | 64.66 | audio-exact |
| 869290__cat-fox_alex__random-rumble-design_20269.wav | 3838388 | 1106130 | 28.82 | audio-exact |
| 869515__cat-fox_alex__modern-rumble-1.wav | 4010068 | 2053580 | 51.21 | audio-exact |
| 869516__cat-fox_alex__modern-rumble-1_2.wav | 10826668 | 5353029 | 49.44 | audio-exact |
| 869538__cat-fox_alex__modern-rumble-2.wav | 15153188 | 8454750 | 55.80 | audio-exact |
| 869539__cat-fox_alex__modern-rumble-2_2.wav | 24325988 | 11737592 | 48.25 | audio-exact |
| 869566__cat-fox_alex__rock-rumble-1.wav | 2022420 | 1118357 | 55.30 | audio-exact |
| 869567__cat-fox_alex__rock-rumble-1_2.wav | 18911140 | 10686414 | 56.51 | audio-exact |
| 870093__cat-fox_alex__woosh_whip-1.wav | 531156 | 56787 | 10.69 | audio-exact |
| 870095__cat-fox_alex__woosh_whip-2.wav | 178356 | 47151 | 26.44 | audio-exact |
| 870096__cat-fox_alex__woosh_whip-3.wav | 68100 | 16974 | 24.93 | audio-exact |
| 871296__cat-fox_alex__heart-beat-1_processeddry.wav | 5690484 | 1703552 | 29.94 | audio-exact |
| 871297__cat-fox_alex__heart-beat-2_processeddry.wav | 5014284 | 1587275 | 31.66 | audio-exact |
| 871354__cat-fox_alex__heart-beat-3_processeddry.wav | 7059788 | 1462105 | 20.71 | audio-exact |
| 871489__cat-fox_alex__porn-slap.wav | 7240388 | 3808576 | 52.60 | audio-exact |
| 871610__cat-fox_alex__lava-rumble.wav | 35528964 | 14437322 | 40.64 | audio-exact |
| 871683__cat-fox_alex__cure-spell.wav | 3025244 | 1008394 | 33.33 | audio-exact |
| 871710__cat-fox_alex__sound-design-impact-1.wav | 329292 | 118225 | 35.90 | audio-exact |
| 871754__cat-fox_alex__liquid-gun.wav | 956140 | 663120 | 69.35 | audio-exact |
| 871755__cat-fox_alex__liquid_one-shot-1.wav | 455292 | 293058 | 64.37 | audio-exact |
| 871757__cat-fox_alex__random-liquid-design-1.wav | 4840092 | 3404842 | 70.35 | audio-exact |
| 871758__cat-fox_alex__random-liquid-design-1_2.wav | 13671116 | 8397114 | 61.42 | audio-exact |
| 871778__cat-fox_alex__random-spell-design-1_attack.wav | 405612 | 274806 | 67.75 | audio-exact |
| 871779__cat-fox_alex__random-spell-design-1_sub-impact.wav | 1033476 | 89984 | 8.71 | audio-exact |
| 871780__cat-fox_alex__random-spell-design-1_top-impact.wav | 1033476 | 602117 | 58.26 | audio-exact |
| 871781__cat-fox_alex__random-spell-design-1.wav | 1078324 | 754779 | 70.00 | audio-exact |
| 872155__cat-fox_alex__sound-design-impact-2.wav | 247388 | 82529 | 33.36 | audio-exact |
| 872629__cat-fox_alex__data-transimit.wav | 20994868 | 6957280 | 33.14 | audio-exact |
| 872722__looplicator__200-bpm-industrial-drum-loop-17450-wav.wav | 1693484 | 742674 | 43.85 | audio-exact |
| 872749__looplicator__120-bpm-industrial-drum-loop-17455-wav.wav | 1411244 | 442013 | 31.32 | audio-exact |
| 873146__cat-fox_alex__random-rumble-design_202610.wav | 12248892 | 5842032 | 47.69 | audio-exact |
| 873147__cat-fox_alex__random-rumble-design_202610.wav | 13915244 | 7357506 | 52.87 | audio-exact |
| 873148__cat-fox_alex__random-rumble-design_202610.wav | 20035692 | 10298792 | 51.40 | audio-exact |
| 873149__looplicator__179-bpm-industrial-drum-loop-17470-wav.wav | 3784304 | 3094446 | 81.77 | audio-exact |
| 873186__looplicator__192-bpm-industrial-drum-loop-17478-wav.wav | 3528080 | 2513274 | 71.24 | audio-exact |
| 873322__looplicator__153-bpm-industrial-drum-loop-17482-wav.wav | 4427376 | 2746133 | 62.03 | audio-exact |
| 873325__looplicator__153-bpm-industrial-drum-loop-17484-wav.wav | 4427376 | 2996916 | 67.69 | audio-exact |
| 873406__looplicator__145-bpm-industrial-drum-loop-17496-wav.wav | 4671640 | 2987687 | 63.95 | audio-exact |
| 873437__looplicator__137-bpm-industrial-drum-loop-17500-wav.wav | 4944432 | 1884111 | 38.11 | audio-exact |
| 873472__looplicator__202-bpm-industrial-drum-loop-17505-wav.wav | 3353424 | 2732753 | 81.49 | audio-exact |
| 873495__cat-fox_alex__bubble-gun-shoot.wav | 2424044 | 1713239 | 70.68 | audio-exact |
| 873526__looplicator__175-bpm-industrial-drum-loop-17514-wav.wav | 3870800 | 2820383 | 72.86 | audio-exact |
| 873676__cat-fox_alex__texture-bubble-spore.wav | 17306212 | 5652705 | 32.66 | audio-exact |
| 873678__cat-fox_alex__texture-bubble-spore-2.wav | 39313692 | 10564342 | 26.87 | audio-exact |
| 873700__cat-fox_alex__texture-spectral-water.wav | 19393092 | 7037051 | 36.29 | audio-exact |
| 873702__cat-fox_alex__texture-spectral-water-2.wav | 19418292 | 6669856 | 34.35 | audio-exact |
| 873766__looplicator__180-bpm-industrial-drum-loop-17549-wav.wav | 3763280 | 2189750 | 58.19 | audio-exact |
| 873781__looplicator__199-bpm-industrial-drum-loop-17555-wav.wav | 3403976 | 2621047 | 77.00 | audio-exact |
| 873827__looplicator__68-bpm-industrial-drum-loop-17562-wav.wav | 4980748 | 1522726 | 30.57 | audio-exact |
| 873886__cat-fox_alex__random-rumble-design_202610.wav | 11299164 | 6241317 | 55.24 | audio-exact |
| 873887__cat-fox_alex__random-rumble-design_202610.wav | 14556268 | 7921233 | 54.42 | audio-exact |
| 873930__looplicator__192-bpm-industrial-drum-loop-17582-wav.wav | 3528080 | 2447974 | 69.39 | audio-exact |
| 874227__looplicator__120-bpm-industrial-drum-loop-17595-wav.wav | 5644880 | 4698503 | 83.23 | audio-exact |
| 874256__looplicator__161-bpm-industrial-drum-loop-17599-wav.wav | 4207384 | 2711085 | 64.44 | audio-exact |
| 874261__looplicator__160-bpm-industrial-drum-loop-17601-wav.wav | 4233680 | 3474558 | 82.07 | audio-exact |
| 874324__cat-fox_alex__ui-interaction-1_1.wav | 1313668 | 433236 | 32.98 | audio-exact |
| 874325__cat-fox_alex__ui-interaction-1_2.wav | 1409740 | 480835 | 34.11 | audio-exact |
| 874326__cat-fox_alex__ui-interaction-1_3.wav | 1800340 | 545545 | 30.30 | audio-exact |
| 874333__cat-fox_alex__mechanic-sound-design-202610.wav | 2291740 | 1017754 | 44.41 | audio-exact |
| 874334__cat-fox_alex__mechanic-sound-design-202610.wav | 12461516 | 5187188 | 41.63 | audio-exact |
| 874335__cat-fox_alex__mechanic-sound-design-202610.wav | 6120564 | 3577406 | 58.45 | audio-exact |
| 874336__cat-fox_alex__mechanic-sound-design-202610.wav | 5745716 | 3445221 | 59.96 | audio-exact |
| 874467__cat-fox_alex__fx-ui-interaction-2.wav | 285188 | 211869 | 74.29 | audio-exact |
| 874491__cat-fox_alex__fx-ui-interaction-3.wav | 1527868 | 894879 | 58.57 | audio-exact |
| 874492__cat-fox_alex__fx-ui-interaction-3_2.wav | 1527868 | 858481 | 56.19 | audio-exact |
| 874493__cat-fox_alex__fx-ui-interaction-3_3.wav | 1527868 | 902626 | 59.08 | audio-exact |
| 874732__timbre__lucky-dip-of-impulse-responses.flac | 145923 | 110819 | 75.94 | audio-exact |
| 874998__looplicator__107-bpm-industrial-drum-loop-17643-wav.wav | 6330696 | 3706805 | 58.55 | audio-exact |
| 875077__looplicator__151-bpm-industrial-drum-loop-17662-wav.wav | 4486016 | 3694520 | 82.36 | audio-exact |
| 875082__looplicator__99-bpm-industrial-drum-loop-17664-wav.wav | 6842264 | 5404518 | 78.99 | audio-exact |
| 875109__timbre__like-a-bad-zoom-call-dropped-packets.flac | 7112188 | 6687480 | 94.03 | audio-exact |
| 875196__looplicator__183-bpm-industrial-drum-loop-17681-wav.wav | 3701592 | 2937262 | 79.35 | audio-exact |
| 875263__cat-fox_alex__random-shutter-fx-design_202610.wav | 3797444 | 1714851 | 45.16 | audio-exact |
| 875323__looplicator__63-bpm-industrial-drum-loop-17692-wav.wav | 10752080 | 7904029 | 73.51 | audio-exact |
| 875334__looplicator__175-bpm-industrial-drum-loop-17697-wav.wav | 3870800 | 2498853 | 64.56 | audio-exact |
| 875336__looplicator__148-bpm-industrial-drum-loop-17698-wav.wav | 2288476 | 739865 | 32.33 | audio-exact |
| 875489__newlocknew__wind_autumnmidnightresidential-areawind-in-the-trees-along-the-fence.wav | 77160456 | 48850493 | 63.31 | audio-exact |
