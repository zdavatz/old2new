#!/usr/bin/env bash
#
# tensordock_batch.sh — Batch upscale davaz.com videos on TensorDock GPU instances
#
# Usage:
#   ./tensordock_batch.sh launch [NUM_INSTANCES]  — Launch instances and start processing (default: 1)
#   ./tensordock_batch.sh status                  — Show status of all instances and progress
#   ./tensordock_batch.sh download [OUTPUT_DIR]   — Download completed videos from instances
#   ./tensordock_batch.sh destroy                 — Destroy all running instances
#   ./tensordock_batch.sh test [VIDEO_ID]          — Launch 1 instance with 1 video to check quality
#   ./tensordock_batch.sh list                    — List all videos to process
#   ./tensordock_batch.sh ssh [INSTANCE_NUM]      — SSH into an instance
#
# Each instance runs a web status page on port 8080 showing per-video progress.
# Job directories use movie titles (not video IDs) for readability.
#
# Requirements:
#   - TensorDock API key: export TENSORDOCK_API_KEY in ~/.bashrc
#   - SSH key: ~/.ssh/id_ed25519.pub (or id_rsa.pub)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
STATE_DIR="$SCRIPT_DIR/.tensordock_batch"
ASSIGNMENTS_DIR="$STATE_DIR/assignments"
COMPLETED_FILE="$STATE_DIR/completed.txt"
OUTPUT_DIR="${OUTPUT_DIR:-$SCRIPT_DIR/enhanced_videos}"

# TensorDock API
TD_API="https://dashboard.tensordock.com/api/v2"
TD_KEY="${TENSORDOCK_API_KEY:-}"

# GPU config (override with env: GPU_MODEL=geforcertx5090-pcie-32gb ./tensordock_batch.sh ...)
GPU_MODEL="${GPU_MODEL:-geforcertx4090-pcie-24gb}"
GPU_DISPLAY="${GPU_DISPLAY:-RTX 4090}"
if [[ "$GPU_MODEL" == *5090* ]]; then
    GPU_DISPLAY="RTX 5090"
fi
NUM_GPUS=1
VCPUS=${VCPUS:-16}   # more vCPUs = faster frame extraction (parallel ffmpeg workers)
RAM_GB=${RAM_GB:-32}  # more RAM for parallel I/O pipeline
STORAGE_GB=250  # default, overridden by estimate_disk_gb()
OS_IMAGE="ubuntu2404"

# ---------- Proven instance profiles ----------
# These are tested configurations with known performance.
# Use as reference when selecting locations for similar workloads.
#
# Profile: SD-4x (SD videos, 4x upscale, no tiling)
#   GPU:       RTX 4090 (24GB VRAM) — geforcertx4090-pcie-24gb
#   CPU:       AMD EPYC 7F72 (2x cores, ~3.2GHz boost) — good single-core for cv2
#   vCPUs:     4 (enough for SD; 16 preferred for faster extraction)
#   RAM:       16GB
#   Disk:      650GB (fits SD videos up to ~55min at 4x)
#   Location:  Ottawa, Ontario, Canada
#   Rate:      ~$0.41/hr
#   Perf:      2.6 fps upscale (640x480 4x, no tiling, 0.3 MP)
#   Best for:  SD videos ≤55min. Frames cleaned between videos.
#
# Profile: HD-2x (HD videos, 2x upscale, needs 5090 to avoid tiling)
#   GPU:       RTX 5090 (32GB VRAM) — geforcertx5090-pcie-32gb
#   vCPUs:     16
#   RAM:       32GB
#   Disk:      1700-3000GB depending on duration
#   Location:  Chubbuck, Idaho (only location with 5090 + large storage)
#   Rate:      ~$0.70-0.80/hr
#   Perf:      ~2+ fps expected (1920x1200 2x, no tiling, 2.3 MP fits 32GB)
#   Best for:  HD videos (1920x1080/1200). DO NOT use RTX 4090 — tiling = 8x slower.
#   Disk rule: ~1700GB for 1h video, ~3000GB for 2h video

# SSH user (TensorDock default)
SSH_USER="user"

# Status web server port
STATUS_PORT=8080

# ---------- Video list (from davaz.com database via YouTube API) ----------
# Format: video_id \t duration_seconds \t definition(hd/sd) \t scale(2/4) \t title
# Sorted by duration descending
read -r -d '' VIDEO_DATA << 'VIDEOEOF' || true
o_nM2N-03UI	6840	hd	2	064_S(T)INGING_BEAUTY_Kamchatka_-_RussianEsub
aUg2dv4XXgM	5829	hd	2	050a_BORN_to_MOVE_X-treams_X-dreams_-_Kazakhstan
mgUOHubnEC8	4062	sd	4	CAMBODIA_DUST_of_LIFE
l8szkLe2eiM	3876	hd	2	077c_Gruz_Koztarsasag_Ahol_Isten_Leszallt_-_Hsub
cWGbmkCvGHA	3872	hd	2	077b_Republik_Georgien_FUSSTRITTE_GOTTES_-_RGAODsub
q_UgL0Pbet8	3872	hd	2	077a_Republic_of_Georgia_WHERE_GOD_LANDED_-_RGAOEsub
NyUEAixkcfQ	3676	sd	4	DPRK_Hero_to_Zero_eyes_wide_-_mouth_shut
tljAVZCj6lw	3602	hd	2	BLUEPRINTS_of_LIFE
fuNkC-JNzEc	3573	sd	4	RUSSIA_TRAPPED_REALITY
tkCxiE1Wlrw	3513	sd	4	KAZAKHSTAN_ETERNAL_HOME
N_Ui88q-gy8	3377	hd	2	072a_Iran_CHADOR_CONDOM_COFFEESHOP_-_FarsiEsub
opavvOpVpUM	3376	hd	2	072b_Iran_KOPFTUCH_PARISER_KAFFEEHAUS_-_FarsiDsub
v1YJC8dMaas	3290	sd	4	JAPAN_High_School_Girls_-_SHIBUYA_-_1712
9lZDEOnRgSU	3200	hd	2	054b_HUMUS_for_HAMAS_-_ArabicHsub
hH4mTkFUKdg	3143	sd	4	FILMTIME_MTV_m2_Hungarian_Television
G9Whw4gJCeY	3132	hd	2	054a_HUMUS_for_HAMAS_Gaza_Strip_-_ArabicEsub
WwHJW39JqPk	3068	hd	2	050c_BORN_to_MOVE_X-treams_X-dreams_-_KHsub
R2zxpDOmjfA	3047	sd	4	CHINA_CCTV_-_PORTRAIT_of_JURG_DA_VAZ
Uv5kGuyJSbc	3010	sd	4	Humus_for_Hamas_subtitles_in_FARSI_24.09.2010
Us1tVOObESU	2746	sd	4	022b_21st_Century_China_4539_feb07
u9LHBYxyj5w	2701	hd	2	VERGANGENHEIT_als_VERMACHTNIS_work_in_progress_1.0
Q9VKZeHaUIc	2541	sd	4	BHUTAN_LIVE
2udCsqGRnyg	2449	sd	4	China_CCTV_-_I_TOOLS_-_EYE_TOOLS_-_a_portrait_of_Jurg_Da_Vaz
rXUD-jxOCuM	2368	hd	2	051a_MISHA_goes_to_SCHOOL_-_REsub
8wqZivWVLZs	2345	sd	4	BUDAPEST_diary
SHFqywMgx1Q	2344	hd	2	007b_BUDAPESTEN_NAPLO_-_H
c62HSWqoxKo	2133	sd	4	RUSSIA_REALtime
Rmjega-OZQg	2126	sd	4	INDIA_O_LUCKY_CALCUTTA
X3J2WOb0FyI	2097	hd	2	030a_ALICE_in_WONDERLAND_-_E
4_gaU85Zzog	2072	hd	2	075a_MAOs_BARBERSHOP_-_chineseEsub
dOF5NTLfmn0	2060	sd	4	A_TESTAMENTUM_Yevgenyi_Burak
v27m8UT4w0M	2059	hd	2	074c_ZAVESHCHANIE_EVGENIYU_BURAKU_-_R
-eTyUFV-KB8	2056	sd	4	DAS_TESTAMENT_-_Yevgenyi_Burak
t6YvdmcGdo8	2042	hd	2	049b_Liebe_Anna_-_D
KHRHsyGIRlI	2033	hd	2	049a_Liebe_Anna_-_E
UExLHdyGfwA	2020	hd	2	049c_Liebe_Anna_-_H
XtzTRGmGWuM	2019	sd	4	fucking_good_KAMCHATKA_kurva_jo_(Hungarian_Edition)
e1e-_J0PzTY	2014	hd	2	Museum_Rundgang_Werni_2024
Ydkc8oZzHBY	2004	sd	4	fucking_good_KAMCHATKA_Ballet_for_Two_Poachers_(English_Edition)
rxHoEF73O5k	2004	sd	4	fucking_good_KAMCHATKA_Ballett_fur_Zwei_Fischdiebe_(Deutsche_Edition)
5Wu0PCEahsg	1948	sd	4	017_SIKKIM_Stories
IBfMLZghog0	1948	hd	2	044_Book_of_Eyes_classic.
MpZicz5Nkrg	1948	sd	4	ISRAEL_ITZHAK_FREY_FIA_(Hungarian_Edition)
oUnnIVKwxv0	1948	hd	2	052d_Itzhak_Frey_Malchik_-_Russian_sub
zPTk4BzdBu8	1945	hd	2	052b_Itzhak_Frey_Son_-_DEsub
roeVmHWKobs	1941	sd	4	RUSSIA_OREG_TO_(Hungarian_Edition)
kUmV1BDCnH4	1939	sd	4	RUSSIA_OLD_PONDS_(English_Edition)
wxJ7SGgb42c	1936	hd	2	052a_Itzhak_Frey_Sohn_-_D
N6jAviwcNmo	1932	sd	4	GAZA_HAMAS_RISE_to_POWER_(English_Edition)
cgHVbxV64VU	1900	hd	2	068_SEX_on_the_STEPS_-_Yellow_Mountains
T0Z6t9zG0rw	1864	sd	4	TRAPPED_REALITY_-_Short_Stories_from_Moscow
CAbPfoe5B_Q	1775	hd	2	045a_SIBERIA_YOU_ARE_MY_LIMIT_-_REsub
WBsY8GGMOBE	1774	sd	4	001a_THE_OTHER_EYE_-_E
KMclVtn2aoc	1762	sd	4	Republic_of_GEORGIA_KMARA_--_ENOUGH_THE_ROSE_REVOLUTION
FaRUcjDYZis	1759	hd	2	001b_MAS_SZEMMEL_-_H
qSgiCb2aJa4	1758	hd	2	070c_KMARA_-_ELEG_ROZSAS_FORRADALOM
hH_E66CkUjQ	1757	hd	2	070b_KMARA_-_GENUG_ROSEN-REVOLUTION_-_GRD
0tapt-cyoSY	1754	sd	4	KAMCHATKA_BEAR_and_FISH_(English_Edition)
7qQ47AhQca4	1753	hd	2	060b_LAZAC_ES_MACKO_PARADICSOM_-_RHsub
Hqa_G12v0Bw	1733	sd	4	004_WORKS_in_PROGRESS
EywN1gXXrAQ	1720	hd	2	085_DEER_VISION_-_Kenya
acjxyO710lw	1694	sd	4	023a_RAISING_CHINA_Interview_H-E
j1J5t163asA	1629	sd	4	KAZAKHSTAN_BALANCE_of_LIFE_BABUSHKA_MASHA_(English_Edition)
vVJAdgh-yxo	1609	sd	4	KAZAKHSTAN_EGYENSULYBAN_BABUSHKA_MASHA_(Hungarian_Edition)
x6S8pAmrv94	1609	hd	2	046b_EGYENSULYBAN_Babushka_Masha_-_H
I-W2hJc6q1U	1597	hd	2	053a_JASMINE_and_OLIVES
nACZdLod2mU	1524	hd	2	030b_ALICE_CSODAORSZAGBAN_-_H
M3D3Gxo4AEQ	1493	sd	4	013_PIG_OPERA
9Rz8jpSUmPM	1434	sd	4	KAMCHATKA_LOST_in_TRANSITION_(English_Edition)
yRT12zdB0V4	1434	hd	2	059b_KRAPIVNAJA_-_LOST_in_TRANSITION_-_RHsub
509HaQXlkL0	1414	hd	2	Republic_of_GEORGIA_TeaTime_in_TBILISI
w2zIO_8S3Ek	1387	sd	4	Republic_of_GEORGIA_drunk_GOD
8SvgnUHDdTU	1356	sd	4	008_PICKPEN
Kv7e-5ii-Ew	1344	sd	4	RUSSIA_DEAD_END
0DNzxYaPVPs	1341	sd	4	KAZAKHSTAN_ARAL_SEA_-_Moments_for_Monuments
4bLA0adDPaI	1339	sd	4	Palestine_HAIDER_ABDELSHAFI_-_last_interview
NllpHjWcq1k	1321	hd	2	055_Syria_Meat_-_eat_meat_-_meet_A_SACRIFICE
L8kk5d87jnk	1304	hd	2	038b_IFJU_FARKAS_Smolensk_-_H
yPJT2-Bodhw	1295	hd	2	038a_YOUNG_WOLF_Smolensk_-_RG_Esub
AtuAAkLGRPo	1291	sd	4	RettungsMarsch_1913_-_saving_20_seconds_(Long_Version)_2130
cMvVKrXIN9Y	1289	sd	4	024_CITY_in_MAKING
OiVFgnVqVpk	1288	hd	2	053b_JASMINE_and_OLIVES_-_Hsub
Z3JLO-Vpk5U	1282	sd	4	ARCHITECTURE_of_a_TRIAL
BXdKRjSVsvg	1188	sd	4	DPRK_Dongan_-_Pyongyang_uncensored_footage
ZsfwCuMHFVw	1158	sd	4	010b_SUITE_702_-_H
Y3Zb55v7sM4	1146	sd	4	RUSSIA_SUITE_702
-wKR0jcUVMU	1140	hd	2	Suchen_Schnuffeln_Scharren_Zupacken
KR9bAUPmCPc	1140	hd	2	FUNdaziun_DA_VAZ_mumaints_dal_temp
bnbaapXPOjg	1139	sd	4	085_c_Maasai_People
k3nEp6-h5ZA	1138	hd	2	MACH_aus_dem_VERGANGENEN_was_Du_KANNST
lA-HJkXDE2A	1106	sd	4	GAZA_35_Million_Dollars_in_a_Suitcase_Gaza_in_Pain
0LdSpIR6x2I	1086	hd	2	078_The_FURNITURE_in_AFGHANISTAN_-_AfghanRussianEnglish
NKXxfXK8qbI	1080	sd	4	HONGKONG_MORPHOPOLIS
aefe1fn7Kf0	1052	hd	2	Base_Camp_Everest_Top_of_the_world
6hUUJNMZKDw	1029	sd	4	KAMCHATKA_TOLBACHIK_1975-2005
FPuC6oD03Ic	986	hd	2	TIBET_Sera_Monastery_Debating_Monks
4_-BxRL1vFs	977	sd	4	005_pf-ERDE_am_HIMMEL
qeNigfxHkEo	937	hd	2	033_The_Takami_Family_Tokyo
1W9mjMtxVSw	934	sd	4	015_Last_Night_I_was_Two_Cats
csxaS-i3wtw	928	hd	2	061b_Volcano_TOLBACHIK_-_time_change
uFI77oD4K_k	928	hd	2	061a_TOLBACHIK_-_Ludmillas_Volcano_-_E
i8XzcDXpBoY	909	sd	4	CALL_OF_THE_SNOUT
GSiput8wrVE	908	sd	4	016_CABAGGE_TALES
n4Vn38iOsw0	907	hd	2	079a_Iran_HANDSHAKE_--_sometimes_KISSES_-_FarsiEsub
sQdMKkCuXkc	907	hd	2	079b_Iran_KEZFOGAS_--_es_neha_csok_-_FarsiHsub
Da3-JcXMnzQ	900	sd	4	PIGstile
PPCRMgkcBFc	888	sd	4	PIG_OPERA
1iN1Fvei5Jk	848	hd	2	082a_Syria_DEIR_ez-ZOR_CAMELS_for_BABIES_-_ArabicEsub
IDZ3VoE3bNo	848	hd	2	082b_Syria_Teveket_Gyerekekert_-_ArabicHsub
d6ph7n4k35Y	831	hd	2	Pick_and_Pen
FnlKDiLSmA0	824	sd	4	011b_BEZARVA_Moscow_-_Russian_Hungarian_sub
QfQyjfQTPjw	820	hd	2	035_Rettungsmarsch_1913
W8aE4bWqX-U	819	sd	4	011a_BUTIRSKAYA_PRISON_Moscow_-_E
HcWVqIwrBE4	789	hd	2	093_Karpfen_and_Mirage_Winniza_Ukraine
uew-t12ZP5Q	783	hd	2	088_Ukraines_Maidan_Kyiv
JXir0H9XPzY	751	sd	4	009_ChickenPick
n8nej0rLYG4	668	hd	2	080_Zhou_Tie_Hai_MR._CAMEL_-_ChineseEnglish
rX4ADnOa3G4	657	hd	2	006_MTV2_Window_on_Europe_--_Europa_Ablak
UrwKbdcn5DY	628	hd	2	Museum_Da_Vaz_Februar_2024_Santa_Maria
BR5U-miBmt4	610	hd	2	073_WANG_GUANGYI_-_Communism_Pops_-_C
Jp2_TDjkTQo	599	hd	2	TIBET_LHASA_Barkhor_Square_Potala_Palace
AWblTn9NeBQ	593	hd	2	081b_ZENG_FANZHI_EMBER_es_FENEDAD_-_ChineseHsub
35EDsDp4aI0	591	hd	2	081a_ZENG_FANZHI_meat_and_mask_-_ChineseEsub
f1FFHaB68UY	588	hd	2	043_Land_Inside_Me_-_Almaty
FKkFMCWm0HI	560	hd	2	042_Behind_the_Curtain_-_Interview_HE
lBtqIR7eu5A	517	hd	2	084_South_Korea_Penis_Parc_-_KoreanEsub
otseQlbkGRE	504	hd	2	TIBET_Rongbuk_Monastery
CM_Qhcpc1us	492	hd	2	091_Dinner_at_Natasha_and_Petro
mlPRxM0RY3E	486	hd	2	Fundaziun_Da_Vaz_Ein_Einblick
3teIj0Rttzg	417	hd	2	066_The_BEATLES_in_GEORGIA_-_GeorgianEsub
g_ynBcYgNoM	415	hd	2	092_Tatra_Tram_in_Kyiv_Ukraine
-x_aIkSrXFw	391	sd	4	DPRK_Pyongyang_Metro
WPV3ysAX_Dg	391	hd	2	TIBET_till_mill
aNzqomzOaUo	391	hd	2	063_HARD_TALK_Biene_und_Mensch_-_D
Oy6C9xa-IkQ	376	sd	4	86c_Baboons_out_of_Kenya
PVSGvKb2pzU	360	sd	4	DPRK_Overland_from_Panmunjon_to_Pyongyang
aOn12f4HN4E	336	sd	4	083e_DPRK_haircut_in_a_cooperative_state_farm_-_Wonsan
Md53nopQbck	324	hd	2	TIBET_Sandpainting_Mandalas_Kumbum_Monastery
HM8JI7LlMU0	305	hd	2	039a_Kazakh_Television_Interview_-_E
ouzpBuJuCJk	305	hd	2	039b_Kazakh_Television_Interview_-_R
M7pLqiSxJ8c	286	hd	2	Visual_Thinking_-_inner_landscapes_-_out_of_a_black_whole
Tg8RccQYnPA	286	hd	2	094_Welcome_to_Ukraine
41GCHnsBV8Y	280	sd	4	TIBET_Everest_Base_Camp
m4j94BSQMp4	257	sd	4	GIRLS_CHOIR_DPRK
gJe-iNomJLk	232	hd	2	089_Sberbank_Kyiv
0RRuKv3u7dU	228	sd	4	DPRK_GAYAGEUM_-_concert_zither
_dxcOch0CrU	221	sd	4	INTERVIEW_FOKUSZ_RTL_CLUB
V1k1xyzaGzY	220	hd	2	067_SOMETHING_to_SAY_to_the_WORLD_-_Bests_from_Lis_out_of_Kenya
AkAtP0oKCMs	216	sd	4	ACCORDION_LESSON_DPRK
AjiFJbsRYyg	206	sd	4	085a_50_tons_of_wild_life_-_Kenya
86Njsv89dlo	192	hd	2	Wunderblock
hYvlNNcPPEI	165	hd	2	90_Heavenly_Private_Papamobil
4QqadqB5ZSs	159	sd	4	PIANO_PRACTISE_DPRK
nIjgA9_9tgM	158	hd	2	DPRK_KIM_JONG_IL_moviemaker_-_all_about_love
6L-z22_WnvA	145	sd	4	Fly_over_Kamchatka
4sgXo0I4uTI	143	sd	4	DPRK_Swiss_Yodeling_over_Pyongyang
kIIFdZrQuTQ	140	sd	4	Hairdresser_in_Georgia
6JCZA5BMpg0	119	sd	4	DPRK_37th_floor_-_Pyongyang
lCpwATzkfMA	117	sd	4	DRAWING_PRACTISE_DPRK
rNtr5Y7Yaok	112	sd	4	DPRK_Guns_and_Cherry_Blossoms
pdya9ZzlGhg	87	sd	4	DPRK_DRAMA_CLASS
B3p6iPx0EM0	84	hd	2	052ad_ITZHAK_FREY_Gedanken_zu_Gebildeten_Menschen_Erdbeben_FC_Barcelona_-_D
1OGG_98oRqY	78	hd	2	052ac_ITZHAK_FREY_Gedanken_zum_Antisemitismus_in_der_Schweiz_-_D
y81sU3NL3Fo	62	sd	4	DPRK_TRADITIONAL_DANCE
sIK1U7gS-tw	58	sd	4	BRUSH_PAINTING_DPRK
td5h1Gcx0gg	55	sd	4	How_do_you_do_in_DPRK
5AdE5zNRq6w	54	hd	2	DPRK_KIM_DYNASTY_-_teaching_method
28sD05dtKzE	46	hd	2	052ab_ITZHAK_FREY_Gedanken_zur_Grundung_des_Slowakischen_Staates_-_D
wtn4-4bQKsk	44	sd	4	Itzhak_Frey_gondolata_a_Felvidekrol
DR9rSeRX8ug	41	sd	4	054ca_zwolf_sekunden_zensur_HUMUS_for_HAMAS
aV-BtCv_vzs	37	hd	2	052af_David_Frey_Kindheitserinnerungen
T5H9nBJ99SU	29	hd	2	Museum_Elephant
TPxRCZqsrPo	16	hd	2	DONT_LOOK_HERE
uGvmLs3uBys	16	hd	2	FAMILY_LIFE
wzgdTKkEQCo	16	hd	2	to_TOO_to_TOOO_to_TOOOO
-77BxiZaqeQ	15	hd	2	WILL_TO_SURVIVE
0lyreCd3gdY	15	hd	2	PRESENT
2BZktntJTUk	15	hd	2	CHICKEN
4QTgJf8Pr44	15	hd	2	oo
8AW2b8VvDtk	15	hd	2	hard_BEAT
8Nj1D0wWu1I	15	hd	2	WUNDERBLOCK
BLP5KBjSHaU	15	hd	2	HELLO_HELLO
EG4aSSkrKT8	15	hd	2	EVERYHANDIs_different
Gz-ulMPi8R8	15	hd	2	EMOTIONS
J42Hm1gVt_U	15	hd	2	untitled_J42Hm1gVt_U
J5Sn3T0eSFU	15	hd	2	MY_STUDENTS
J9poOhWeG54	15	hd	2	process_isis_process
MKwI2DLtPos	15	hd	2	drip_drip
NqqHRvsDek8	15	hd	2	the_OTHER_EYE
P5eG6xQ1gJg	15	hd	2	CABBAGE_NETWORK
QPQyVHtCyho	15	hd	2	CUTTING-EDGE
RTcR_xd9S_I	15	hd	2	CROSS_BORDERS
RkRryabSdiw	15	hd	2	MATTER_OF_CHANCE
UWMqbzQUfI0	15	hd	2	PICK_AND_PEN
UvrS8IpQSl8	15	hd	2	FORCED_adjustment
_aB3Pc7krPw	15	hd	2	roadside_BHUTAN
aqs-jHb2pnk	15	hd	2	Bei_meiner_Ehre
b1Vst4OlmWY	15	hd	2	WHYWHYWHY
c6soqG3ZYMA	15	hd	2	MAKE-UP
cbAE4XrqJqc	15	hd	2	SINGING_BEAUTY
dqiDqnYOHrU	15	hd	2	nur_fur_DOKUMENT
eaeJozXNBPA	15	hd	2	CHICKENWORLD
ef1zAYI6Akg	15	hd	2	enter_BHUTAN
f4x_p_zRons	15	hd	2	WE_ARE_THE_WORLD
fwSfBzIftJI	15	hd	2	COME_WITH_ME
hFL_Ct-qBRI	15	hd	2	untitled_hFL_Ct-qBRI
iLdhxG2KPoM	15	hd	2	DREAM_WORLD
nt_PzgdjJ2Y	15	hd	2	Its_a_Family_Game
pi9sOSMrEqY	15	hd	2	HEY_HEY
qnQMFzoMsHA	15	hd	2	At_natives_deep_inside_Kamchatka
vpNjxCy99Fs	15	hd	2	WECANNOTIMAGINE
wjAkVoSN8jE	15	hd	2	Itzhak_Frei_working_in_his_own_bakery_in_Mea_Shearim_Jerusalem
xiFVzz_x5PU	15	hd	2	Where_Where_HERE
yt1tQsqYI1s	15	hd	2	kidsKIDSkidsKIDSkids
ytPO9BNAVLg	15	hd	2	for_DISCOVERY
EnnKos4thVc	14	hd	2	PHILOMENA
I0HeJAlbtY4	14	hd	2	FATHER_SAID
R4bK1jJSfQc	14	hd	2	1997_HAND_OVER
UG1krgFORrk	14	hd	2	paphlipffpap-philipffppflapilphfefe
WO3jbQwqrxc	14	hd	2	REFLECTION
TXE3bBaK8zY	13	hd	2	GRILLED_PIG
wAUvvktUoOc	13	hd	2	Whats_your_name
akfCabK-6jA	12	hd	2	MAMA_TALK
YX0zzFIzkW4	11	hd	2	THE_MESSAGE_OF_COLOR
gDSPYgEWlMc	8	hd	2	UNEXPECTEDLY_SIGNIFICANT
UbZzwqQ1ocw	7	hd	2	FISH_n_PISS
VIDEOEOF

# ---------- Helper functions ----------

log() {
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] $*"
}

ensure_state_dir() {
    mkdir -p "$STATE_DIR" "$ASSIGNMENTS_DIR"
    touch "$COMPLETED_FILE"
}

get_ssh_key() {
    local key_file=""
    for f in ~/.ssh/id_ed25519.pub ~/.ssh/id_rsa.pub ~/.ssh/id_ecdsa.pub; do
        if [[ -f "$f" ]]; then
            key_file="$f"
            break
        fi
    done
    if [[ -z "$key_file" ]]; then
        echo "ERROR: No SSH public key found in ~/.ssh/" >&2
        exit 1
    fi
    cat "$key_file"
}

# Pre-launch check: estimate disk, check VRAM/tiling, recommend GPU
# Fetches exact resolution via yt-dlp --dump-json (no download)
# Outputs disk GB to stdout; logs details + warnings to stderr
# Sets global NEEDS_5090=1 if any video requires >24GB VRAM to avoid tiling
NEEDS_5090=0
estimate_disk_gb() {
    local video_list="$1"  # tab-separated: vid\tscale\ttitle\tduration
    local max_gb=0
    NEEDS_5090=0
    while IFS=$'\t' read -r vid scale title duration; do
        [[ -z "$vid" ]] && continue
        # Fetch video resolution via yt-dlp --dump-json (no download)
        local info
        info=$(yt-dlp --dump-json --no-download "https://www.youtube.com/watch?v=$vid" 2>/dev/null)
        local width height fps
        width=$(echo "$info" | python3 -c "import json,sys; d=json.load(sys.stdin); print(d.get('width',720))" 2>/dev/null || echo 720)
        height=$(echo "$info" | python3 -c "import json,sys; d=json.load(sys.stdin); print(d.get('height',480))" 2>/dev/null || echo 480)
        fps=$(echo "$info" | python3 -c "import json,sys; d=json.load(sys.stdin); print(int(d.get('fps',25)))" 2>/dev/null || echo 25)

        # VRAM/tiling check — same thresholds as enhance_gpu.py
        # RTX 4090 (24GB): safe up to 1.6 MP without tiling
        # RTX 5090 (32GB): safe up to 2.0 MP without tiling
        local mpixels_x10=$(( width * height * 10 / 1000000 ))  # 10x for integer math
        local tiling_info="no tiling"
        if [[ $mpixels_x10 -gt 16 ]]; then  # > 1.6 MP
            if [[ $mpixels_x10 -gt 20 ]]; then  # > 2.0 MP — needs 5090 or tiling
                tiling_info="NEEDS 5090 (32GB) to avoid tiling — will be ~8x slower on 4090!"
                NEEDS_5090=1
            else
                tiling_info="tiling on 4090 (~3-4x slower), OK on 5090"
                NEEDS_5090=1
            fi
        fi

        # Disk estimate: PNG compression ~2.5x, 10% safety + 5GB overhead
        local total_frames=$(( duration * fps ))
        local input_bytes=$(( width * height * 3 ))
        local output_bytes=$(( width * scale * height * scale * 3 ))
        local input_gb=$(( total_frames * input_bytes * 10 / 25 / 1073741824 ))
        local output_gb=$(( total_frames * output_bytes * 10 / 25 / 1073741824 ))
        local video_gb=$(( (input_gb + output_gb) * 110 / 100 + 5 ))
        if [[ $video_gb -gt $max_gb ]]; then
            max_gb=$video_gb
        fi
        local mpixels_str=$(( mpixels_x10 / 10 )).$(( mpixels_x10 % 10 ))
        log "  $title: ${width}x${height} (${mpixels_str} MP) @ ${fps}fps, ${duration}s, ${scale}x" >&2
        log "    Disk: ~${video_gb}GB | VRAM: ${tiling_info}" >&2
    done <<< "$video_list"
    # Add overhead: OS/deps + 20% safety, round to 50
    max_gb=$(( max_gb * 120 / 100 + 50 ))
    max_gb=$(( (max_gb + 49) / 50 * 50 ))
    # Output: disk_gb needs_5090 (space-separated)
    echo "$max_gb $NEEDS_5090"
}

td_api() {
    local method="$1"
    local endpoint="$2"
    shift 2
    curl -s -X "$method" "${TD_API}${endpoint}" \
        -H "Authorization: Bearer $TD_KEY" \
        -H "Content-Type: application/json" \
        -H "Accept: application/json" \
        "$@"
}

# Find cheapest location with RTX 4090 and enough storage
find_best_location() {
    local min_storage=${1:-$STORAGE_GB}
    td_api GET /locations | python3 -c "
import json, sys
data = json.load(sys.stdin)
locations = data.get('data', {}).get('locations', [])
best = []
for loc in locations:
    for gpu in loc['gpus']:
        if gpu['v0Name'] == '$GPU_MODEL':
            if gpu['resources']['max_storage_gb'] >= $min_storage:
                best.append({
                    'id': loc['id'],
                    'city': loc['city'],
                    'state': loc.get('stateprovince', ''),
                    'country': loc['country'],
                    'gpu_price': gpu['price_per_hr'],
                    'max_gpus': gpu['max_count'],
                    'max_storage': gpu['resources']['max_storage_gb'],
                    'max_vcpus': gpu['resources']['max_vcpus'],
                    'max_ram': gpu['resources']['max_ram_gb'],
                    'dedicated_ip': gpu['network_features']['dedicated_ip_available'],
                    'pricing': gpu['pricing'],
                })
best.sort(key=lambda x: x['gpu_price'])
for b in best:
    vcpus = min($VCPUS, b['max_vcpus']) if b['max_vcpus'] > 0 else $VCPUS
    ram = min($RAM_GB, b['max_ram']) if b['max_ram'] > 0 else $RAM_GB
    total = b['gpu_price'] + b['pricing']['per_vcpu_hr'] * vcpus + b['pricing']['per_gb_ram_hr'] * ram + b['pricing']['per_gb_storage_hr'] * $STORAGE_GB
    print(f\"{b['id']}\t{b['city']}, {b['state']}, {b['country']}\t\${total:.3f}/hr\t{b['max_storage']}GB\t{b['dedicated_ip']}\t{vcpus}\t{ram}\")
"
}

# Create a TensorDock instance
create_instance() {
    local name="$1"
    local location_id="$2"
    local cloud_init_script="$3"
    local use_vcpus="${4:-$VCPUS}"
    local use_ram="${5:-$RAM_GB}"
    local ssh_key
    ssh_key=$(get_ssh_key)

    # Write cloud-init script to temp file for safe JSON encoding
    local tmp_script
    tmp_script=$(mktemp)
    echo "$cloud_init_script" > "$tmp_script"

    local tmp_key
    tmp_key=$(mktemp)
    echo "$ssh_key" > "$tmp_key"

    local payload
    payload=$(python3 -c "
import json
with open('$tmp_script') as f:
    script_content = f.read()
with open('$tmp_key') as f:
    ssh_key = f.read().strip()
data = {
    'data': {
        'type': 'virtualmachine',
        'attributes': {
            'name': '$name',
            'type': 'virtualmachine',
            'image': '$OS_IMAGE',
            'resources': {
                'vcpu_count': $use_vcpus,
                'ram_gb': $use_ram,
                'storage_gb': $STORAGE_GB,
                'gpus': {
                    '$GPU_MODEL': {
                        'count': $NUM_GPUS
                    }
                }
            },
            'location_id': '$location_id',
            'port_forwards': [
                {'internal_port': 22, 'external_port': 22, 'protocol': 'tcp'},
                {'internal_port': 8080, 'external_port': 8080, 'protocol': 'tcp'}
            ],
            'ssh_key': ssh_key,
            'cloud_init': {
                'write_files': [
                    {
                        'path': '/root/setup.sh',
                        'content': script_content,
                        'owner': 'root:root',
                        'permissions': '0755'
                    }
                ],
                'runcmd': ['bash /root/setup.sh > /root/enhance.log 2>&1 &']
            }
        }
    }
}
print(json.dumps(data))
")
    rm -f "$tmp_script" "$tmp_key"

    td_api POST /instances -d "$payload"
}

# ---------- Generate setup script that runs on each instance ----------
generate_setup_script() {
    local video_list="$1"  # newline-separated: video_id\tscale\ttitle\tduration
    local instance_label="${2:-davaz-enhance}"
    local instance_location="${3:-TensorDock}"
    local instance_cost="${4:-0}"
    local instance_id="${5:-unknown}"
    # Strip $ and /hr from cost if present
    instance_cost="${instance_cost//\$/}"
    instance_cost="${instance_cost//\/hr/}"

    cat << 'SETUP_HEADER'
#!/bin/bash
set -e

export DEBIAN_FRONTEND=noninteractive

echo "=== Setup started at $(date) ==="

# Install minimal system packages (no apt update — saves 3-5 min on throwaway instances)
echo "Installing system packages..."
apt-get install -y --no-install-recommends python3-pip ffmpeg 2>/dev/null || {
    # Only run apt-get update if install fails (package cache missing)
    apt-get update -qq && apt-get install -y --no-install-recommends python3-pip ffmpeg
}

# Install Python dependencies
echo "Installing Python dependencies..."
# Ubuntu 24.04 requires --break-system-packages for system pip
PIP="pip install --break-system-packages -q"

# Fix typing_extensions (broken RECORD file on Ubuntu 24.04)
$PIP --ignore-installed typing_extensions 2>&1 || true

# Install PyTorch with CUDA first (not bundled with base Ubuntu)
# Detect GPU arch to pick correct CUDA version
GPU_ARCH=$(nvidia-smi --query-gpu=compute_cap --format=csv,noheader 2>/dev/null | head -1 | tr -d '.')
if [ "${GPU_ARCH:-0}" -ge 120 ]; then
    echo "Blackwell GPU detected (sm_${GPU_ARCH}) — using PyTorch with CUDA 12.8"
    $PIP torch torchvision --index-url https://download.pytorch.org/whl/cu128 2>&1 || true
else
    $PIP torch torchvision --index-url https://download.pytorch.org/whl/cu121 2>&1 || true
fi

# Install realesrgan and deps
$PIP realesrgan yt-dlp "numpy==1.26.4" "basicsr==1.4.2" 2>&1 || true

# Fix opencv: remove full version, install headless (avoids libGL issues)
pip uninstall --break-system-packages -y opencv-python opencv-contrib-python 2>/dev/null || true
$PIP "opencv-python-headless==4.10.0.84" 2>&1 || true

# Re-pin numpy (opencv install may have upgraded it)
$PIP "numpy==1.26.4" 2>&1 || true

# Patch basicsr for newer torchvision (functional_tensor removed)
DEGRADATIONS_FILE=$(python3 -c "import importlib.util; print(importlib.util.find_spec('basicsr').submodule_search_locations[0])" 2>/dev/null)/data/degradations.py
if [ -f "$DEGRADATIONS_FILE" ] && grep -q "functional_tensor" "$DEGRADATIONS_FILE"; then
    sed -i 's/from torchvision.transforms.functional_tensor import rgb_to_grayscale/from torchvision.transforms.functional import rgb_to_grayscale/' "$DEGRADATIONS_FILE"
    echo "Patched basicsr for torchvision compatibility"
fi

# Install static ffmpeg (system ffmpeg may be old or missing codecs)
if ! ffmpeg -version 2>/dev/null | grep -q "7\."; then
    echo "Installing static ffmpeg 7.x..."
    curl -sL https://johnvansickle.com/ffmpeg/releases/ffmpeg-release-amd64-static.tar.xz | tar xJ -C /tmp/
    cp /tmp/ffmpeg-*-amd64-static/ffmpeg /usr/local/bin/ffmpeg
    cp /tmp/ffmpeg-*-amd64-static/ffprobe /usr/local/bin/ffprobe
fi

echo "Dependencies installed."

# Speed test
echo "Testing download speed..."
SPEED_START=$(date +%s%N)
curl -sL "https://speed.cloudflare.com/__down?bytes=10000000" -o /dev/null
SPEED_END=$(date +%s%N)
SPEED_MS=$(( (SPEED_END - SPEED_START) / 1000000 ))
if [ "$SPEED_MS" -gt 0 ]; then
    SPEED_MBPS=$(( 10 * 8 * 1000 / SPEED_MS ))
    echo "Download speed: ${SPEED_MBPS} Mbps"
else
    echo "Speed test too fast to measure"
fi

# Download enhance_gpu.py
curl -sL "https://raw.githubusercontent.com/zdavatz/old2new/main/enhance_gpu.py" -o /root/enhance_gpu.py
echo "enhance_gpu.py downloaded."

SETUP_HEADER

    # Generate the video queue JSON for the status server
    echo ""
    echo "# Write video queue for status server"
    echo 'cat > /root/video_queue.json << '"'"'QUEUEJSON'"'"''
    echo '['
    local first=true
    while IFS=$'\t' read -r vid scale title duration; do
        [[ -z "$vid" ]] && continue
        if [[ "$first" == "true" ]]; then
            first=false
        else
            echo ','
        fi
        local display_title="${title//_/ }"
        printf '  {"id": "%s", "scale": %s, "title": "%s", "display_title": "%s", "duration": %s}' "$vid" "$scale" "$title" "$display_title" "$duration"
    done <<< "$video_list"
    echo ''
    echo ']'
    echo 'QUEUEJSON'
    echo ""

    # Write instance metadata for dashboard display
    echo "# Write instance metadata for dashboard"
    echo "cat > /root/instance_meta.json << 'METAJSON'"
    echo "{"
    echo "  \"label\": \"$instance_label\","
    echo "  \"location\": \"$instance_location\","
    echo "  \"cost_per_hr\": $instance_cost,"
    echo "  \"provider\": \"tensordock\","
    echo "  \"instance_id\": \"$instance_id\""
    echo "}"
    echo "METAJSON"
    echo ""

    # Download and start the status server
    echo "# Download and start status server"
    echo 'curl -sL "https://raw.githubusercontent.com/zdavatz/old2new/main/status_server.py?$(date +%s)" -o /root/status_server.py'
    echo 'python3 /root/status_server.py &'
    echo 'echo "Status server started on port 8080"'
    echo ""

    # Append video processing loop
    echo "# --- Process videos ---"
    echo 'VIDEOS=('
    while IFS=$'\t' read -r vid scale title duration; do
        [[ -z "$vid" ]] && continue
        echo "  \"$vid $scale $title\""
    done <<< "$video_list"
    echo ')'
    echo ""
    cat << 'SETUP_LOOP'
TOTAL=${#VIDEOS[@]}
DONE=0
FAILED=0

for entry in "${VIDEOS[@]}"; do
    vid=$(echo "$entry" | cut -d' ' -f1)
    scale=$(echo "$entry" | cut -d' ' -f2)
    title=$(echo "$entry" | cut -d' ' -f3-)

    DONE=$((DONE + 1))
    echo ""
    echo "=========================================="
    echo "Processing video $DONE/$TOTAL: $title ($vid, scale=${scale}x)"
    echo "Started at: $(date)"
    echo "=========================================="

    # Check if already completed
    if [[ -f "/root/jobs/$title/${title}_${scale}x.mkv" ]] || [[ -f "/root/jobs/$title/enhanced_${scale}x.mkv" ]]; then
        echo "Already completed, skipping."
        continue
    fi

    # Run enhance_gpu.py with --job-name to use movie title as directory name
    URL="https://www.youtube.com/watch?v=$vid"
    if python3 /root/enhance_gpu.py "$URL" "$scale" --job-name "$title"; then
        echo "SUCCESS: $title completed at $(date)"
        echo "$vid $title" >> /root/completed.txt

        # Clean up frames to save disk for next video
        rm -rf "/root/jobs/$title/frames_in" "/root/jobs/$title/frames_out"
        echo "Cleaned up frames for $title"
    else
        echo "FAILED: $title at $(date)"
        echo "$vid $title FAILED" >> /root/completed.txt
        FAILED=$((FAILED + 1))

        # Clean up failed job to reclaim disk
        rm -rf "/root/jobs/$title/frames_in" "/root/jobs/$title/frames_out"
    fi
done

echo ""
echo "=========================================="
echo "ALL DONE at $(date)"
echo "Completed: $((DONE - FAILED))/$TOTAL  Failed: $FAILED"
echo "=========================================="
SETUP_LOOP
}

# ---------- Assign videos to instances using greedy load-balancing ----------
assign_videos() {
    local num_instances=$1

    local -a vids durations scales titles
    local i=0
    while IFS=$'\t' read -r vid dur def scale title; do
        [[ -z "$vid" ]] && continue
        if grep -qF "$vid" "$COMPLETED_FILE" 2>/dev/null; then
            continue
        fi
        vids+=("$vid")
        durations+=("$dur")
        scales+=("$scale")
        titles+=("$title")
        i=$((i + 1))
    done <<< "$VIDEO_DATA"

    local total=${#vids[@]}
    if [[ $total -eq 0 ]]; then
        log "No videos to process (all completed?)"
        return 1
    fi

    log "Distributing $total videos across $num_instances instances..."

    local -a instance_load
    for ((j=0; j<num_instances; j++)); do
        instance_load[$j]=0
        > "$ASSIGNMENTS_DIR/instance_${j}.txt"
    done

    for ((i=0; i<total; i++)); do
        local min_idx=0
        local min_load=${instance_load[0]}
        for ((j=1; j<num_instances; j++)); do
            if [[ ${instance_load[$j]} -lt $min_load ]]; then
                min_idx=$j
                min_load=${instance_load[$j]}
            fi
        done

        echo -e "${vids[$i]}\t${scales[$i]}\t${titles[$i]}\t${durations[$i]}" >> "$ASSIGNMENTS_DIR/instance_${min_idx}.txt"
        instance_load[$min_idx]=$((min_load + ${durations[$i]}))
    done

    for ((j=0; j<num_instances; j++)); do
        local count
        count=$(wc -l < "$ASSIGNMENTS_DIR/instance_${j}.txt")
        local hours=$((${instance_load[$j]} / 3600))
        log "  Instance $j: $count videos, ~${hours}h of video content"
    done
}

# ---------- Commands ----------

cmd_launch() {
    local num_instances=${1:-1}

    ensure_state_dir

    if [[ -z "$TD_KEY" ]]; then
        echo "ERROR: TENSORDOCK_API_KEY not set."
        echo "Add to ~/.bashrc: export TENSORDOCK_API_KEY=\"your-token\""
        exit 1
    fi

    log "Launching $num_instances TensorDock instances with $GPU_DISPLAY..."

    assign_videos "$num_instances" || exit 1

    # Estimate max disk needs across all instance assignments
    log "Estimating disk needs..."
    local max_disk=250
    NEEDS_5090=0
    for ((j=0; j<num_instances; j++)); do
        local assignment_file="$ASSIGNMENTS_DIR/instance_${j}.txt"
        local estimate_result
        estimate_result=$(estimate_disk_gb "$(cat "$assignment_file")")
        local est=$(echo "$estimate_result" | awk '{print $1}')
        local needs=$(echo "$estimate_result" | awk '{print $2}')
        if [[ $est -gt $max_disk ]]; then
            max_disk=$est
        fi
        if [[ "${needs:-0}" -eq 1 ]]; then
            NEEDS_5090=1
        fi
    done
    STORAGE_GB=$max_disk
    log "Disk estimate: ${STORAGE_GB}GB needed (largest single video)"

    if [[ "${NEEDS_5090:-0}" -eq 1 && "$GPU_MODEL" == *4090* ]]; then
        log ""
        log "WARNING: Some videos need >24GB VRAM to avoid tiling."
        log "  Switching to RTX 5090 automatically."
        GPU_MODEL="geforcertx5090-pcie-32gb"
        GPU_DISPLAY="RTX 5090"
    fi
    log ""

    # Find best locations with enough storage
    log "Searching for $GPU_DISPLAY locations with ${STORAGE_GB}GB storage..."
    local locations
    locations=$(find_best_location)

    if [[ -z "$locations" ]]; then
        echo "No suitable locations found with $GPU_DISPLAY and ${STORAGE_GB}GB storage."
        exit 1
    fi

    local available
    available=$(echo "$locations" | wc -l)
    log "Found $available locations:"
    echo "$locations" | while IFS=$'\t' read -r lid loc price storage dedicated; do
        log "  $loc — $price (${storage} max, dedicated_ip=$dedicated)"
    done

    if [[ "$available" -lt "$num_instances" ]]; then
        log "WARNING: Only $available locations available, reducing to $available instances"
        num_instances=$available
        assign_videos "$num_instances"
    fi

    local idx=0
    while IFS=$'\t' read -r location_id location_name price storage dedicated loc_vcpus loc_ram; do
        [[ $idx -ge $num_instances ]] && break

        local assignment_file="$ASSIGNMENTS_DIR/instance_${idx}.txt"
        local video_list
        video_list=$(cat "$assignment_file")

        local instance_name="davaz-enhance-${idx}"
        local setup_file="$STATE_DIR/setup_${idx}.sh"
        generate_setup_script "$video_list" "$instance_name" "$location_name" "$price" "pending" > "$setup_file"

        log "Creating instance $idx at $location_name ($price, ${loc_vcpus} vCPUs)..."

        local result
        result=$(create_instance "$instance_name" "$location_id" "$(cat "$setup_file")" "$loc_vcpus" "$loc_ram")

        local instance_id
        instance_id=$(echo "$result" | python3 -c "
import json, sys
data = json.load(sys.stdin)
d = data.get('data', data)
if isinstance(d, dict):
    print(d.get('id', 'unknown'))
else:
    print('unknown')
" 2>/dev/null || echo "unknown")

        if [[ "$instance_id" != "unknown" && -n "$instance_id" ]]; then
            echo "$instance_id" > "$STATE_DIR/instance_${idx}.id"
            log "  Instance $idx: ID=$instance_id"
        else
            log "  WARNING: Failed to create instance $idx"
            log "  Response: $result"
        fi

        idx=$((idx + 1))
    done <<< "$locations"

    echo ""
    log "Launch complete! $idx instances created."
    log ""
    log "Monitor progress:"
    log "  ./tensordock_batch.sh status     — terminal overview"
    log "  ./tensordock_batch.sh ssh 0      — SSH into instance 0"
    log "  ./tensordock_batch.sh download   — download completed videos"
    log "  ./tensordock_batch.sh destroy    — clean up when done"
}

cmd_test() {
    local video_id="${1:-}"

    ensure_state_dir

    if [[ -z "$TD_KEY" ]]; then
        echo "ERROR: TENSORDOCK_API_KEY not set."
        exit 1
    fi

    # If no video ID given, pick CAMBODIA DUST of LIFE — long SD video, 4x
    if [[ -z "$video_id" ]]; then
        video_id="mgUOHubnEC8"
        log "No video ID specified. Using: CAMBODIA DUST of LIFE (1:07:42, SD, 4x)"
        log "You can specify any video ID: ./tensordock_batch.sh test VIDEO_ID"
        log "Run './tensordock_batch.sh list' to see all available videos."
        echo ""
    fi

    # Find the video in our list
    local vid_line
    vid_line=$(echo "$VIDEO_DATA" | grep "^${video_id}"$'\t' || true)
    if [[ -z "$vid_line" ]]; then
        echo "ERROR: Video ID '$video_id' not found in the video list."
        echo "Run './tensordock_batch.sh list' to see available video IDs."
        exit 1
    fi

    local vid dur def scale title
    IFS=$'\t' read -r vid dur def scale title <<< "$vid_line"
    local h=$((dur / 3600))
    local m=$(( (dur % 3600) / 60 ))
    local dur_str
    if [[ $h -gt 0 ]]; then
        dur_str="${h}h ${m}m"
    else
        dur_str="${m}m"
    fi
    local display_title="${title//_/ }"

    log "=== Test Run: Quality Check ==="
    log "Video:    $display_title"
    log "ID:       $vid"
    log "Duration: $dur_str ($def)"
    log "Scale:    ${scale}x"
    log ""

    # Write single-video assignment
    echo -e "${vid}\t${scale}\t${title}\t${dur}" > "$ASSIGNMENTS_DIR/instance_0.txt"

    # Estimate disk needs and check VRAM/tiling before launching
    log "Pre-launch check..."
    local video_list
    video_list=$(cat "$ASSIGNMENTS_DIR/instance_0.txt")
    local estimate_result
    estimate_result=$(estimate_disk_gb "$video_list")
    STORAGE_GB=$(echo "$estimate_result" | awk '{print $1}')
    NEEDS_5090=$(echo "$estimate_result" | awk '{print $2}')
    log "Disk estimate: ${STORAGE_GB}GB needed"

    # Auto-switch to RTX 5090 if video needs it to avoid slow tiling
    if [[ "${NEEDS_5090:-0}" -eq 1 && "$GPU_MODEL" == *4090* ]]; then
        log ""
        log "WARNING: This video needs >24GB VRAM to avoid tiling."
        log "  RTX 4090 (24GB) would use tiling → ~8x slower (0.3 fps vs 2.5 fps)"
        log "  RTX 5090 (32GB) can process without tiling → full speed"
        log "  Switching to RTX 5090 automatically."
        GPU_MODEL="geforcertx5090-pcie-32gb"
        GPU_DISPLAY="RTX 5090"
    fi
    log ""

    # Find cheapest location with enough storage
    log "Searching for cheapest $GPU_DISPLAY with ${STORAGE_GB}GB storage..."
    local locations
    locations=$(find_best_location)

    if [[ -z "$locations" ]]; then
        echo "No suitable locations found."
        exit 1
    fi

    local location_id location_name price _storage _dedicated loc_vcpus loc_ram
    IFS=$'\t' read -r location_id location_name price _storage _dedicated loc_vcpus loc_ram <<< "$(echo "$locations" | head -1)"

    log "Selected: $location_name @ $price (${loc_vcpus} vCPUs, ${loc_ram}GB RAM)"
    log ""

    # Generate setup script with metadata
    local video_list
    video_list=$(cat "$ASSIGNMENTS_DIR/instance_0.txt")
    local setup_file="$STATE_DIR/setup_test.sh"
    generate_setup_script "$video_list" "davaz-test" "$location_name" "$price" "pending" > "$setup_file"

    log "Creating test instance..."
    local result
    result=$(create_instance "davaz-test" "$location_id" "$(cat "$setup_file")" "$loc_vcpus" "$loc_ram")

    local instance_id
    instance_id=$(echo "$result" | python3 -c "
import json, sys
data = json.load(sys.stdin)
d = data.get('data', data)
if isinstance(d, dict):
    print(d.get('id', ''))
else:
    print('')
" 2>/dev/null || echo "")

    if [[ -n "$instance_id" ]]; then
        echo "$instance_id" > "$STATE_DIR/instance_test.id"
        log "Instance created: ID=$instance_id"
    else
        log "ERROR: Failed to create instance"
        log "Response: $result"
        exit 1
    fi

    echo ""
    log "Test instance launched!"
    log ""
    log "Next steps:"
    log "  1. ./tensordock_batch.sh status          — check progress"
    log "  2. ./tensordock_batch.sh ssh 0            — SSH in to monitor"
    log "  3. ./tensordock_batch.sh download         — download enhanced video when done"
    log "  4. ./tensordock_batch.sh destroy          — destroy test instance"
}

cmd_status() {
    ensure_state_dir

    if [[ -z "$TD_KEY" ]]; then
        echo "ERROR: TENSORDOCK_API_KEY not set."
        exit 1
    fi

    echo "=== Da Vaz Video Enhancement — TensorDock Status ==="
    echo ""

    local instance_list
    instance_list=$(td_api GET /instances)

    local instance_ids
    instance_ids=$(echo "$instance_list" | python3 -c "
import json, sys
data = json.load(sys.stdin)
for inst in data.get('data', []):
    print(inst['id'])
" 2>/dev/null)

    if [[ -z "$instance_ids" ]]; then
        echo "No instances running."
        return
    fi

    # Fetch full details for each instance
    echo "$instance_ids" | while read -r iid; do
        [[ -z "$iid" ]] && continue
        local details
        details=$(td_api GET "/instances/$iid")
        echo "$details" | python3 -c "
import json, sys
inst = json.load(sys.stdin)
iid = inst.get('id', '?')
name = inst.get('name', '?')
status = inst.get('status', '?')
ip = inst.get('ipAddress', '?')
ports = inst.get('portForwards', [])
resources = inst.get('resources', {})
rate = inst.get('rateHourly', '?')
gpus = resources.get('gpus', {})
gpu_name = list(gpus.keys())[0].replace('geforcertx', 'RTX ').replace('-pcie-', ' ').replace('gb','GB') if gpus else '?'

ssh_port = 22
web_port = 8080
for p in ports:
    if p.get('internal_port') == 22:
        ssh_port = p.get('external_port', 22)
    if p.get('internal_port') == 8080:
        web_port = p.get('external_port', 8080)

print(f'  Instance: {name} [{status}]')
print(f'    ID:        {iid}')
print(f'    GPU:       {gpu_name}')
print(f'    IP:        {ip}')
print(f'    Rate:      \${rate}/hr')
print(f'    SSH:       ssh -p {ssh_port} user@{ip}')
if status == 'running':
    print(f'    Dashboard: http://{ip}:{web_port}')
print()
" 2>/dev/null
    done

    # Show saved instance IDs
    if ls "$STATE_DIR"/instance_*.id &>/dev/null; then
        echo "--- Tracked Instances ---"
        for f in "$STATE_DIR"/instance_*.id; do
            local label
            label=$(basename "$f" .id)
            echo "  $label: $(cat "$f")"
        done
        echo ""
    fi
}

cmd_ssh() {
    local instance_num="${1:-0}"

    if [[ -z "$TD_KEY" ]]; then
        echo "ERROR: TENSORDOCK_API_KEY not set."
        exit 1
    fi

    # Find instance ID
    local id_file="$STATE_DIR/instance_${instance_num}.id"
    if [[ ! -f "$id_file" ]]; then
        # Try test instance
        id_file="$STATE_DIR/instance_test.id"
    fi
    if [[ ! -f "$id_file" ]]; then
        echo "ERROR: No instance found. Run 'launch' or 'test' first."
        exit 1
    fi

    local instance_id
    instance_id=$(cat "$id_file")

    local details
    details=$(td_api GET "/instances/$instance_id")

    local ssh_info
    ssh_info=$(echo "$details" | python3 -c "
import json, sys
data = json.load(sys.stdin)
ip = data.get('ipAddress', '')
ports = data.get('portForwards', [])
ssh_port = 22
for p in ports:
    if p.get('internal_port') == 22:
        ssh_port = p.get('external_port', 22)
print(f'{ip} {ssh_port}')
" 2>/dev/null)

    local ip port
    read -r ip port <<< "$ssh_info"

    if [[ -z "$ip" || "$ip" == "None" ]]; then
        echo "Instance not ready yet (no IP assigned). Try again in a moment."
        exit 1
    fi

    log "Connecting to instance $instance_num ($instance_id) at $ip:$port..."
    exec ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -p "$port" "$SSH_USER@$ip"
}

cmd_download() {
    local out_dir="${1:-$OUTPUT_DIR}"
    mkdir -p "$out_dir"

    if [[ -z "$TD_KEY" ]]; then
        echo "ERROR: TENSORDOCK_API_KEY not set."
        exit 1
    fi

    local instances
    instances=$(td_api GET /instances)

    echo "$instances" | python3 -c "
import json, sys
data = json.load(sys.stdin)
for inst in data.get('data', []):
    ip = inst.get('ipAddress', '')
    ports = inst.get('portForwards', [])
    ssh_port = 22
    for p in ports:
        if p.get('internal_port') == 22:
            ssh_port = p.get('external_port', 22)
    name = inst.get('name', '')
    print(f'{ip}\t{ssh_port}\t{name}')
" 2>/dev/null | while IFS=$'\t' read -r ip port name; do
        [[ -z "$ip" || "$ip" == "None" ]] && continue
        log "Downloading from $name ($ip:$port)..."

        # Find completed videos
        local completed
        completed=$(ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -p "$port" "$SSH_USER@$ip" \
            'find ~/jobs -name "*_*x.mkv" -type f 2>/dev/null' || true)

        if [[ -z "$completed" ]]; then
            log "  No completed videos found on $name"
            continue
        fi

        echo "$completed" | while read -r remote_path; do
            local filename
            filename=$(basename "$remote_path")
            if [[ -f "$out_dir/$filename" ]]; then
                log "  Already downloaded: $filename"
                continue
            fi
            log "  Downloading: $filename"
            scp -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -P "$port" \
                "${SSH_USER}@${ip}:${remote_path}" "$out_dir/" || log "  FAILED: $filename"
        done
    done

    log "Downloads saved to: $out_dir"
}

cmd_destroy() {
    if [[ -z "$TD_KEY" ]]; then
        echo "ERROR: TENSORDOCK_API_KEY not set."
        exit 1
    fi

    local instances
    instances=$(td_api GET /instances)

    local ids
    ids=$(echo "$instances" | python3 -c "
import json, sys
data = json.load(sys.stdin)
for inst in data.get('data', []):
    print(inst.get('id', ''))
" 2>/dev/null)

    if [[ -z "$ids" ]]; then
        echo "No instances to destroy."
        return
    fi

    echo "$ids" | while read -r iid; do
        [[ -z "$iid" ]] && continue
        log "Destroying instance $iid..."
        td_api DELETE "/instances/$iid" || log "  Failed to destroy $iid"
    done

    # Clean up state
    rm -f "$STATE_DIR"/instance_*.id

    log "All instances destroyed."
}

cmd_list() {
    echo "=== Da Vaz Video List ($(echo "$VIDEO_DATA" | grep -c $'\t') videos) ==="
    echo ""
    printf "%-14s  %7s  %3s  %5s  %s\n" "VIDEO_ID" "LENGTH" "DEF" "SCALE" "TITLE"
    printf "%-14s  %7s  %3s  %5s  %s\n" "----------" "------" "---" "-----" "-----"

    echo "$VIDEO_DATA" | while IFS=$'\t' read -r vid dur def scale title; do
        [[ -z "$vid" ]] && continue
        local h=$((dur / 3600))
        local m=$(( (dur % 3600) / 60 ))
        local s=$((dur % 60))
        local dur_str
        if [[ $h -gt 0 ]]; then
            dur_str=$(printf "%d:%02d:%02d" "$h" "$m" "$s")
        else
            dur_str=$(printf "%d:%02d" "$m" "$s")
        fi
        local display_title="${title//_/ }"
        printf "%-14s  %7s  %3s  %4sx  %s\n" "$vid" "$dur_str" "$def" "$scale" "$display_title"
    done
}

# ---------- Main ----------

case "${1:-help}" in
    launch)  cmd_launch "${2:-1}" ;;
    test)    cmd_test "${2:-}" ;;
    status)  cmd_status ;;
    ssh)     cmd_ssh "${2:-0}" ;;
    download) cmd_download "${2:-}" ;;
    destroy) cmd_destroy ;;
    list)    cmd_list ;;
    help|*)
        echo "Usage: $0 <command> [args]"
        echo ""
        echo "Commands:"
        echo "  launch [N]        Launch N instances (default: 1)"
        echo "  test [VIDEO_ID]   Launch 1 instance with 1 video for quality check"
        echo "  status            Show instance status and progress"
        echo "  ssh [N]           SSH into instance N (default: 0)"
        echo "  download [DIR]    Download completed videos"
        echo "  destroy           Destroy all instances"
        echo "  list              List all videos to process"
        ;;
esac
