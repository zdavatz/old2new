#!/usr/bin/env bash
#
# vast_batch.sh — Batch upscale davaz.com videos on vast.ai using parallel RTX 4090 instances
#
# Usage:
#   ./vast_batch.sh launch [NUM_INSTANCES]  — Launch instances and start processing (default: 4)
#   ./vast_batch.sh status                  — Show status of all instances and progress
#   ./vast_batch.sh download [OUTPUT_DIR]   — Download completed videos from instances
#   ./vast_batch.sh destroy                 — Destroy all running instances
#   ./vast_batch.sh test [VIDEO_ID]          — Launch 1 instance with 1 video to check quality
#   ./vast_batch.sh list                    — List all videos to process
#   ./vast_batch.sh resume                  — Resume processing on existing instances
#
# Each instance runs a web status page on port 8080 showing per-video progress.
# Job directories use movie titles (not video IDs) for readability.
#
# Requirements:
#   - vastai CLI: pip install vastai
#   - vast.ai API key: vastai set api-key YOUR_KEY
#   - SSH key attached to vast.ai account

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
STATE_DIR="$SCRIPT_DIR/.vast_batch"
ASSIGNMENTS_DIR="$STATE_DIR/assignments"
COMPLETED_FILE="$STATE_DIR/completed.txt"
OUTPUT_DIR="${OUTPUT_DIR:-$SCRIPT_DIR/enhanced_videos}"

# Docker image for vast.ai instances
DOCKER_IMAGE="pytorch/pytorch:2.1.0-cuda12.1-cudnn8-runtime"

# GPU config
GPU_NAME="RTX_4090"
NUM_GPUS=1
DISK_GB=250

# Status web server port (inside container)
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

get_video_count() {
    echo "$VIDEO_DATA" | grep -c $'\t' || echo 0
}

get_instance_ids() {
    if ls "$STATE_DIR"/instance_*.id &>/dev/null; then
        cat "$STATE_DIR"/instance_*.id
    fi
}

get_running_instances() {
    vastai show instances --raw 2>/dev/null | python3 -c "
import json, sys
data = json.load(sys.stdin)
for inst in data:
    status = inst.get('actual_status', 'unknown')
    iid = inst['id']
    gpu = inst.get('gpu_name', '?')
    gpu_ram_mb = inst.get('gpu_totalram', inst.get('gpu_ram', 0))
    gpu_ram = gpu_ram_mb / 1024
    gpu_util = inst.get('gpu_util', 0)
    gpu_temp = inst.get('gpu_temp', 0)
    gpu_mem_bw = inst.get('gpu_mem_bw', 0)
    gpu_arch = inst.get('gpu_arch', '?')
    vmem_usage = inst.get('vmem_usage', 0)
    cpu_name = inst.get('cpu_name', '?')
    cpu_cores = inst.get('cpu_cores_effective', inst.get('cpu_cores', 0))
    cpu_ram_mb = inst.get('cpu_ram', 0)
    cpu_ram = cpu_ram_mb / 1024
    cpu_util = inst.get('cpu_util', 0)
    disk_space = inst.get('disk_space', 0)
    disk_usage = inst.get('disk_usage', 0)
    disk_util = inst.get('disk_util', 0)
    disk_name = inst.get('disk_name', '?')
    disk_bw = inst.get('disk_bw', 0)
    pcie_bw = inst.get('pcie_bw', 0)
    pci_gen = inst.get('pci_gen', '?')
    mobo = inst.get('mobo_name', None) or 'N/A'
    inet_down = inst.get('inet_down', 0)
    inet_up = inst.get('inet_up', 0)
    cuda = inst.get('cuda_max_good', '?')
    driver = inst.get('driver_version', '?')
    reliability = inst.get('reliability2', 0) * 100
    country = inst.get('geolocation', inst.get('country_code', '?'))
    dph = inst.get('dph_total', 0)
    host = inst.get('ssh_host', '?')
    port = inst.get('ssh_port', '?')
    ports = inst.get('ports', {})
    web_port = ''
    if ports and '8080/tcp' in ports:
        mapping = ports['8080/tcp']
        if isinstance(mapping, list) and len(mapping) > 0:
            web_port = str(mapping[0].get('HostPort', ''))
        elif isinstance(mapping, dict):
            web_port = str(mapping.get('HostPort', ''))
    label = inst.get('label', '')
    uptime = inst.get('uptime_mins', 0) or 0
    if uptime >= 60:
        uptime_str = f'{uptime/60:.1f}h'
    else:
        uptime_str = f'{uptime:.0f}m'
    print(f'{iid}\t{gpu}\t{gpu_ram:.1f}\t{gpu_util:.0f}\t{gpu_temp:.0f}\t{gpu_mem_bw:.0f}\t{gpu_arch}\t{vmem_usage:.1f}\t{cpu_name}\t{cpu_cores:.0f}\t{cpu_ram:.1f}\t{cpu_util:.0f}\t{disk_space:.0f}\t{disk_usage:.0f}\t{disk_util:.0f}\t{disk_name}\t{disk_bw:.0f}\t{pcie_bw:.1f}\t{pci_gen}\t{mobo}\t{inet_down:.0f}\t{inet_up:.0f}\t{cuda}\t{driver}\t{reliability:.1f}\t{country}\t{dph:.4f}\t{host}\t{port}\t{status}\t{web_port}\t{label}\t{uptime_str}')
" 2>/dev/null || true
}

# ---------- Status web server (Python, runs on each instance) ----------
generate_status_server() {
    cat << 'STATUSEOF'
#!/usr/bin/env python3
"""Lightweight status web server for vast.ai enhancement instances."""
import http.server
import json
import os
import glob
import time
from datetime import datetime, timedelta

JOBS_DIR = os.path.expanduser("~/jobs")
QUEUE_FILE = os.path.expanduser("~/video_queue.json")
PORT = 8080

class StatusHandler(http.server.BaseHTTPRequestHandler):
    def log_message(self, format, *args):
        pass  # suppress access logs

    def do_GET(self):
        if self.path == "/api/status":
            self.send_json(self.get_status())
        elif self.path == "/":
            self.send_html(self.render_page())
        else:
            self.send_error(404)

    def send_json(self, data):
        body = json.dumps(data).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", len(body))
        self.end_headers()
        self.wfile.write(body)

    def send_html(self, html):
        body = html.encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", len(body))
        self.end_headers()
        self.wfile.write(body)

    def get_status(self):
        queue = []
        if os.path.exists(QUEUE_FILE):
            with open(QUEUE_FILE) as f:
                queue = json.load(f)

        videos = []
        for entry in queue:
            vid = entry["id"]
            title = entry["title"]
            scale = entry["scale"]
            duration = entry["duration"]
            job_dir = os.path.join(JOBS_DIR, title)

            status = "queued"
            progress = 0
            total_frames = 0
            done_frames = 0
            eta = ""
            enhanced_file = os.path.join(job_dir, f"enhanced_{scale}x.mkv")

            if os.path.exists(enhanced_file):
                status = "done"
                progress = 100
                size_mb = os.path.getsize(enhanced_file) / (1024*1024)
                eta = f"{size_mb:.0f} MB"
            elif os.path.isdir(os.path.join(job_dir, "frames_out")):
                status = "upscaling"
                frames_in = glob.glob(os.path.join(job_dir, "frames_in", "frame_*.png"))
                frames_out = glob.glob(os.path.join(job_dir, "frames_out", "frame_*.png"))
                total_frames = len(frames_in)
                done_frames = len(frames_out)
                if total_frames > 0:
                    progress = round(done_frames / total_frames * 100, 1)
            elif os.path.isdir(os.path.join(job_dir, "frames_in")):
                status = "extracting"
                frames_in = glob.glob(os.path.join(job_dir, "frames_in", "frame_*.png"))
                total_frames = len(frames_in)
                if total_frames > 0:
                    status = "upscaling"
            elif os.path.exists(os.path.join(job_dir, "original.mkv")):
                status = "downloaded"

            videos.append({
                "id": vid,
                "title": title,
                "scale": scale,
                "duration": duration,
                "status": status,
                "progress": progress,
                "total_frames": total_frames,
                "done_frames": done_frames,
                "eta": eta,
            })

        # Read last lines of enhance.log
        log_tail = ""
        log_path = os.path.expanduser("~/enhance.log")
        if os.path.exists(log_path):
            with open(log_path, "rb") as f:
                f.seek(0, 2)
                size = f.tell()
                f.seek(max(0, size - 4096))
                log_tail = f.read().decode("utf-8", errors="replace")
                log_tail = "\n".join(log_tail.split("\n")[-30:])

        total = len(videos)
        done = sum(1 for v in videos if v["status"] == "done")
        active = [v for v in videos if v["status"] == "upscaling"]

        return {
            "total": total,
            "done": done,
            "active": active[0]["title"] if active else None,
            "videos": videos,
            "log_tail": log_tail,
            "timestamp": datetime.now().isoformat(),
        }

    def render_page(self):
        return """<!DOCTYPE html>
<html><head>
<meta charset="utf-8">
<title>Da Vaz Video Enhancement</title>
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta http-equiv="refresh" content="30">
<style>
  * { box-sizing: border-box; margin: 0; padding: 0; }
  body { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
         background: #0f172a; color: #e2e8f0; padding: 20px; }
  h1 { font-size: 1.5rem; margin-bottom: 4px; color: #f8fafc; }
  .subtitle { color: #94a3b8; margin-bottom: 20px; font-size: 0.9rem; }
  .summary { display: flex; gap: 16px; margin-bottom: 24px; flex-wrap: wrap; }
  .card { background: #1e293b; border-radius: 8px; padding: 16px 20px; min-width: 140px; }
  .card .num { font-size: 2rem; font-weight: 700; color: #38bdf8; }
  .card .label { color: #94a3b8; font-size: 0.85rem; }
  table { width: 100%; border-collapse: collapse; background: #1e293b; border-radius: 8px; overflow: hidden; }
  th { text-align: left; padding: 10px 12px; background: #334155; color: #94a3b8;
       font-size: 0.8rem; text-transform: uppercase; letter-spacing: 0.05em; }
  td { padding: 8px 12px; border-top: 1px solid #334155; font-size: 0.9rem; }
  tr:hover { background: #1e3a5f; }
  .bar-bg { background: #334155; border-radius: 4px; height: 20px; position: relative; overflow: hidden; min-width: 120px; }
  .bar-fg { height: 100%; border-radius: 4px; transition: width 0.5s; }
  .bar-text { position: absolute; top: 0; left: 8px; line-height: 20px; font-size: 0.75rem; font-weight: 600; }
  .status { display: inline-block; padding: 2px 8px; border-radius: 4px; font-size: 0.75rem; font-weight: 600; }
  .status-done { background: #065f46; color: #6ee7b7; }
  .status-upscaling { background: #1e3a5f; color: #38bdf8; }
  .status-extracting { background: #713f12; color: #fbbf24; }
  .status-downloaded { background: #3b0764; color: #c084fc; }
  .status-queued { background: #334155; color: #94a3b8; }
  .log { background: #0f172a; border: 1px solid #334155; border-radius: 8px; padding: 12px;
         font-family: monospace; font-size: 0.75rem; max-height: 300px; overflow-y: auto;
         white-space: pre-wrap; color: #94a3b8; margin-top: 20px; }
  .title-col { max-width: 350px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  a { color: #38bdf8; text-decoration: none; }
  a:hover { text-decoration: underline; }
</style>
</head><body>
<h1>Da Vaz Video Enhancement</h1>
<p class="subtitle">Real-ESRGAN AI Upscaling &mdash; auto-refreshes every 30s</p>
<div id="app">Loading...</div>
<script>
async function update() {
  try {
    const r = await fetch('/api/status');
    const d = await r.json();
    const app = document.getElementById('app');
    const active = d.videos.filter(v => v.status === 'upscaling');
    const done = d.videos.filter(v => v.status === 'done');
    const queued = d.videos.filter(v => v.status === 'queued');
    const other = d.videos.filter(v => !['done','upscaling','queued'].includes(v.status));

    let h = `<div class="summary">
      <div class="card"><div class="num">${d.total}</div><div class="label">Total Videos</div></div>
      <div class="card"><div class="num" style="color:#6ee7b7">${done.length}</div><div class="label">Completed</div></div>
      <div class="card"><div class="num" style="color:#38bdf8">${active.length}</div><div class="label">Upscaling Now</div></div>
      <div class="card"><div class="num" style="color:#94a3b8">${queued.length}</div><div class="label">Queued</div></div>
    </div>`;

    h += `<table><thead><tr>
      <th>#</th><th>Title</th><th>Scale</th><th>Duration</th><th>Status</th><th>Progress</th>
    </tr></thead><tbody>`;

    const sorted = [...active, ...other, ...queued.slice(0,1), ...done.reverse(), ...queued.slice(1)];
    // Actually show: active first, then non-done/non-queued, then done, then queued
    const order = [...active, ...other, ...done, ...queued];
    order.forEach((v, i) => {
      const dur = v.duration >= 3600
        ? `${Math.floor(v.duration/3600)}:${String(Math.floor((v.duration%3600)/60)).padStart(2,'0')}:${String(v.duration%60).padStart(2,'0')}`
        : `${Math.floor(v.duration/60)}:${String(v.duration%60).padStart(2,'0')}`;
      const barColor = v.status === 'done' ? '#6ee7b7' : '#38bdf8';
      const ytUrl = 'https://www.youtube.com/watch?v=' + v.id;
      h += `<tr>
        <td>${i+1}</td>
        <td class="title-col"><a href="${ytUrl}" target="_blank">${v.title.replace(/_/g, ' ')}</a></td>
        <td>${v.scale}x</td>
        <td>${dur}</td>
        <td><span class="status status-${v.status}">${v.status}</span></td>
        <td><div class="bar-bg"><div class="bar-fg" style="width:${v.progress}%;background:${barColor}"></div>
            <span class="bar-text">${v.status==='done' ? v.eta : v.progress > 0 ? v.done_frames+'/'+v.total_frames : ''}</span></div></td>
      </tr>`;
    });
    h += '</tbody></table>';

    if (d.log_tail) {
      h += '<div class="log">' + d.log_tail.replace(/</g,'&lt;') + '</div>';
    }
    h += '<p style="margin-top:12px;color:#64748b;font-size:0.75rem">Updated: ' + d.timestamp + '</p>';
    app.innerHTML = h;
  } catch(e) {
    document.getElementById('app').innerHTML = '<p>Error loading status: ' + e + '</p>';
  }
}
update();
setInterval(update, 30000);
</script>
</body></html>"""

if __name__ == "__main__":
    server = http.server.HTTPServer(("0.0.0.0", PORT), StatusHandler)
    print(f"Status server running on port {PORT}")
    server.serve_forever()
STATUSEOF
}

# ---------- Onstart script that runs on each vast.ai instance ----------
generate_onstart_script() {
    local video_list="$1"  # newline-separated: video_id\tscale\ttitle\tduration

    cat << 'ONSTART_HEADER'
#!/bin/bash
set -e

export DEBIAN_FRONTEND=noninteractive

# Log everything
exec > >(tee -a /root/enhance.log) 2>&1
echo "=== Onstart script started at $(date) ==="

# Install dependencies
echo "Installing dependencies..."
apt-get update -qq
apt-get install -y -qq ffmpeg > /dev/null 2>&1

pip install -q realesrgan gfpgan yt-dlp "numpy<2" 2>/dev/null || {
    pip install -q "torchvision==0.15.2" "basicsr==1.4.2" 2>/dev/null
    pip uninstall -y opencv-python opencv-contrib-python 2>/dev/null || true
    pip install -q opencv-python-headless 2>/dev/null
    pip install -q realesrgan gfpgan yt-dlp "numpy<2" 2>/dev/null
}

echo "Dependencies installed."

# Speed test
echo "Testing download speed..."
SPEED_START=$(date +%s%N)
curl -sL "https://speed.cloudflare.com/__down?bytes=10000000" -o /dev/null
SPEED_END=$(date +%s%N)
SPEED_MS=$(( (SPEED_END - SPEED_START) / 1000000 ))
SPEED_MBPS=$(( 10 * 8 * 1000 / SPEED_MS ))
echo "Download speed: ${SPEED_MBPS} Mbps"
if [ "$SPEED_MBPS" -lt 50 ]; then
    echo "WARNING: Very slow download speed (${SPEED_MBPS} Mbps)!"
fi

# Download enhance_gpu.py
curl -sL "https://raw.githubusercontent.com/zdavatz/old2new/main/enhance_gpu.py" -o /root/enhance_gpu.py
echo "enhance_gpu.py downloaded."

ONSTART_HEADER

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
        printf '  {"id": "%s", "scale": %s, "title": "%s", "duration": %s}' "$vid" "$scale" "$title" "$duration"
    done <<< "$video_list"
    echo ''
    echo ']'
    echo 'QUEUEJSON'
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
    cat << 'ONSTART_LOOP'
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
ONSTART_LOOP
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
    local num_instances=${1:-4}

    ensure_state_dir

    log "Launching $num_instances vast.ai instances with $GPU_NAME..."

    if [[ ! -f "$HOME/.config/vastai/vast_api_key" ]]; then
        echo "ERROR: No vast.ai API key found."
        echo "Run: vastai set api-key YOUR_API_KEY"
        exit 1
    fi

    assign_videos "$num_instances" || exit 1

    log "Searching for $GPU_NAME offers..."
    local offers
    offers=$(vastai search offers "gpu_name=$GPU_NAME num_gpus=$NUM_GPUS reliability>0.95 disk_space>=$DISK_GB inet_down>100 rented=False direct_port_count>=1" -o 'dph_total' --raw 2>/dev/null)

    local available
    available=$(echo "$offers" | python3 -c "import json,sys; d=json.load(sys.stdin); print(len(d))" 2>/dev/null || echo 0)

    if [[ "$available" -lt "$num_instances" ]]; then
        log "WARNING: Only $available offers available, need $num_instances"
        if [[ "$available" -eq 0 ]]; then
            echo "No suitable offers found. Try adjusting GPU_NAME or DISK_GB."
            exit 1
        fi
        num_instances=$available
        log "Reducing to $num_instances instances"
        assign_videos "$num_instances"
    fi

    local offer_ids
    offer_ids=$(echo "$offers" | python3 -c "
import json, sys
DISK_GB = $DISK_GB
data = json.load(sys.stdin)
# Calculate real cost including storage for requested disk
for d in data:
    stor = d.get('storage_cost', 0) or 0
    d['real_dph'] = d.get('dph_base', d.get('dph_total', 0)) + stor * DISK_GB / 730
data.sort(key=lambda x: x.get('real_dph', 999))
for d in data[:$num_instances]:
    print(f\"{d['id']}\t{d.get('real_dph',0):.4f}\t{d.get('gpu_name','?')}\")
")

    log "Selected offers:"
    echo "$offer_ids" | while IFS=$'\t' read -r oid price gpu; do
        log "  Offer $oid: $gpu @ \$$price/hr"
    done

    local idx=0
    while IFS=$'\t' read -r offer_id price gpu; do
        local assignment_file="$ASSIGNMENTS_DIR/instance_${idx}.txt"
        local video_list
        video_list=$(cat "$assignment_file")

        local onstart_file="$STATE_DIR/onstart_${idx}.sh"
        generate_onstart_script "$video_list" > "$onstart_file"

        log "Creating instance $idx from offer $offer_id ($gpu @ \$$price/hr)..."

        local result
        result=$(vastai create instance "$offer_id" \
            --image "$DOCKER_IMAGE" \
            --disk "$DISK_GB" \
            --ssh \
            --direct \
            --env "-p ${STATUS_PORT}:${STATUS_PORT}" \
            --onstart "$onstart_file" \
            --label "davaz-enhance-$idx" \
            --raw 2>&1) || true

        local instance_id
        instance_id=$(echo "$result" | python3 -c "
import json, sys
try:
    data = json.load(sys.stdin)
    if isinstance(data, dict) and 'new_contract' in data:
        print(data['new_contract'])
    elif isinstance(data, dict) and 'id' in data:
        print(data['id'])
    else:
        print(data)
except:
    print(sys.stdin.read().strip())
" 2>/dev/null || echo "$result")

        if [[ -n "$instance_id" && "$instance_id" != "null" ]]; then
            echo "$instance_id" > "$STATE_DIR/instance_${idx}.id"
            log "  Instance $idx: ID=$instance_id"
        else
            log "  WARNING: Failed to create instance $idx: $result"
        fi

        idx=$((idx + 1))
    done <<< "$(echo "$offer_ids" | head -n "$num_instances")"

    echo ""
    log "Launch complete! $idx instances created."
    log ""
    log "Monitor progress:"
    log "  ./vast_batch.sh status     — terminal overview + web status URLs"
    log "  ./vast_batch.sh download   — download completed videos"
    log "  ./vast_batch.sh destroy    — clean up when done"
    log ""
    log "Each instance serves a live status page on port $STATUS_PORT."
    log "Run './vast_batch.sh status' in a minute to see the URLs."
}

cmd_test() {
    local video_id="${1:-}"

    ensure_state_dir

    # If no video ID given, pick a short SD video (~5 min) for a meaningful test
    if [[ -z "$video_id" ]]; then
        # Default: pick a medium-short SD video for best quality-check value
        # 009_ChickenPick (JXir0H9XPzY, 12:31, sd, 4x) — long enough to judge, short enough to finish fast
        video_id="JXir0H9XPzY"
        log "No video ID specified. Using default test video: 009_ChickenPick (12:31, SD, 4x)"
        log "You can specify any video ID: ./vast_batch.sh test VIDEO_ID"
        log "Run './vast_batch.sh list' to see all available videos."
        echo ""
    fi

    # Find the video in our list
    local vid_line
    vid_line=$(echo "$VIDEO_DATA" | grep "^${video_id}"$'\t' || true)
    if [[ -z "$vid_line" ]]; then
        echo "ERROR: Video ID '$video_id' not found in the video list."
        echo "Run './vast_batch.sh list' to see available video IDs."
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

    if [[ ! -f "$HOME/.config/vastai/vast_api_key" ]]; then
        echo "ERROR: No vast.ai API key found."
        echo "Run: vastai set api-key YOUR_API_KEY"
        exit 1
    fi

    # Write single-video assignment
    > "$ASSIGNMENTS_DIR/instance_0.txt"
    echo -e "${vid}\t${scale}\t${title}\t${dur}" > "$ASSIGNMENTS_DIR/instance_0.txt"

    # Generate onstart script for this single video
    local video_list
    video_list=$(cat "$ASSIGNMENTS_DIR/instance_0.txt")
    local onstart_file="$STATE_DIR/onstart_test.sh"
    generate_onstart_script "$video_list" > "$onstart_file"

    # Search for cheapest RTX 4090
    log "Searching for cheapest $GPU_NAME..."
    local offers
    offers=$(vastai search offers "gpu_name=$GPU_NAME num_gpus=$NUM_GPUS reliability>0.95 disk_space>=$DISK_GB inet_down>100 rented=False direct_port_count>=1" -o 'dph_total' --raw 2>/dev/null)

    local available
    available=$(echo "$offers" | python3 -c "import json,sys; d=json.load(sys.stdin); print(len(d))" 2>/dev/null || echo 0)

    if [[ "$available" -eq 0 ]]; then
        echo "No suitable offers found."
        exit 1
    fi

    local offer_id price gpu
    read -r offer_id price gpu <<< "$(echo "$offers" | python3 -c "
import json, sys
DISK_GB = $DISK_GB
data = json.load(sys.stdin)
for d in data:
    stor = d.get('storage_cost', 0) or 0
    d['real_dph'] = d.get('dph_base', d.get('dph_total', 0)) + stor * DISK_GB / 730
data.sort(key=lambda x: x.get('real_dph', 999))
d = data[0]
print(f\"{d['id']}\t{d.get('real_dph',0):.4f}\t{d.get('gpu_name','?')}\")
")"

    log "Selected: $gpu @ \$$price/hr (incl. storage for ${DISK_GB}GB)"

    # Estimate cost for this single video
    # Rough: 30fps * duration * 1s/frame for 4x, 0.5s for 2x
    local est_seconds
    if [[ "$scale" -eq 4 ]]; then
        est_seconds=$((dur * 30))
    else
        est_seconds=$((dur * 15))
    fi
    local est_hours=$(echo "$est_seconds / 3600" | bc -l 2>/dev/null || echo "?")
    local est_cost=$(echo "$est_hours * $price" | bc -l 2>/dev/null || echo "?")
    log "Estimated processing: ~${est_hours%.*}h, ~\$${est_cost%.*}"
    log ""

    log "Creating test instance..."
    local result
    result=$(vastai create instance "$offer_id" \
        --image "$DOCKER_IMAGE" \
        --disk "$DISK_GB" \
        --ssh \
        --direct \
        --env "-p ${STATUS_PORT}:${STATUS_PORT}" \
        --onstart "$onstart_file" \
        --label "davaz-test" \
        --raw 2>&1) || true

    local instance_id
    instance_id=$(echo "$result" | python3 -c "
import json, sys
try:
    data = json.load(sys.stdin)
    if isinstance(data, dict) and 'new_contract' in data:
        print(data['new_contract'])
    elif isinstance(data, dict) and 'id' in data:
        print(data['id'])
    else:
        print(data)
except:
    print(sys.stdin.read().strip())
" 2>/dev/null || echo "$result")

    if [[ -n "$instance_id" && "$instance_id" != "null" ]]; then
        echo "$instance_id" > "$STATE_DIR/instance_test.id"
        log "Instance created: ID=$instance_id"
    else
        log "ERROR: Failed to create instance: $result"
        exit 1
    fi

    echo ""
    log "Test instance launched!"
    log ""
    log "Next steps:"
    log "  1. ./vast_batch.sh status          — check progress + get status page URL"
    log "  2. Open status URL in browser       — watch live progress"
    log "  3. ./vast_batch.sh download         — download the enhanced video when done"
    log "  4. Check quality of the output"
    log "  5. ./vast_batch.sh destroy          — destroy test instance"
    log "  6. ./vast_batch.sh launch 4         — launch full batch if quality is good"
}

cmd_status() {
    ensure_state_dir

    echo "=== Da Vaz Video Enhancement — Status ==="
    echo ""

    echo "--- Instances ---"
    local instances
    instances=$(get_running_instances)
    if [[ -z "$instances" ]]; then
        echo "No instances found."
        echo ""
        vastai show instances 2>/dev/null || echo "(none)"
        return
    fi

    echo "$instances" | while IFS=$'\t' read -r id gpu gpu_ram gpu_util gpu_temp gpu_mem_bw gpu_arch vmem_usage cpu_name cpu_cores cpu_ram cpu_util disk_space disk_usage disk_util disk_name disk_bw pcie_bw pci_gen mobo inet_down inet_up cuda driver reliability country dph host ssh_port status web_port label uptime; do
        local url=""
        if [[ -n "$web_port" && "$web_port" != "0" ]]; then
            url="http://${host}:${web_port}"
        elif [[ "$status" == "running" ]]; then
            url="(port mapping pending...)"
        fi
        echo "  Instance $id  [$status]  Label: $label  Uptime: $uptime"
        echo "    GPU:      $gpu  ${gpu_ram}GB VRAM (${vmem_usage}GB used)  |  Arch: $gpu_arch  Mem BW: ${gpu_mem_bw} GB/s"
        echo "              Util: ${gpu_util}%  Temp: ${gpu_temp}°C  |  PCIe Gen${pci_gen} @ ${pcie_bw} GB/s"
        echo "    CPU:      $cpu_name"
        echo "              ${cpu_cores} cores  ${cpu_ram}GB RAM  |  Util: ${cpu_util}%"
        echo "    Mobo:     $mobo"
        echo "    Disk:     ${disk_usage}GB / ${disk_space}GB (${disk_util}%)  |  BW: ${disk_bw} MB/s"
        echo "              [$disk_name]"
        echo "    Network:  Down: ${inet_down} Mbps  Up: ${inet_up} Mbps"
        echo "    CUDA:     $cuda  Driver: $driver"
        echo "    Location: $country  Reliability: ${reliability}%"
        echo "    Cost:     \$${dph}/hr"
        echo "    SSH:      ssh -p $ssh_port root@$host"
        echo "    Dashboard: $url"
        echo ""
    done

    echo ""
    echo "--- Per-Instance Progress ---"
    local instance_ids
    instance_ids=$(get_instance_ids)
    if [[ -z "$instance_ids" ]]; then
        echo "No tracked instances. Run './vast_batch.sh launch' first."
        return
    fi

    for iid in $instance_ids; do
        echo ""
        echo "Instance $iid:"
        local log_tail
        log_tail=$(vastai logs "$iid" --tail 30 2>/dev/null || echo "(unable to fetch logs)")
        echo "$log_tail" | grep -E "Processing video|SUCCESS|FAILED|ALL DONE|Benchmark|remaining|Dependencies|started" | tail -8
    done

    echo ""

    local total_videos
    total_videos=$(get_video_count)
    local local_completed
    local_completed=$(wc -l < "$COMPLETED_FILE" 2>/dev/null || echo 0)
    echo "--- Overall ---"
    echo "Total videos: $total_videos"
    echo "Downloaded locally: $local_completed"
    echo ""
    echo "Open the Status Page URLs above in your browser for live per-video progress."
}

cmd_download() {
    local dest="${1:-$OUTPUT_DIR}"
    mkdir -p "$dest"

    ensure_state_dir

    log "Downloading completed videos to $dest/"

    local instance_ids
    instance_ids=$(get_instance_ids)
    if [[ -z "$instance_ids" ]]; then
        log "No tracked instances."
        return
    fi

    for iid in $instance_ids; do
        log "Checking instance $iid for completed videos..."

        local files
        files=$(vastai execute "$iid" "find /root/jobs -name 'enhanced_*.mkv' -type f 2>/dev/null" 2>/dev/null || true)

        if [[ -z "$files" ]]; then
            log "  No completed videos on instance $iid"
            continue
        fi

        echo "$files" | while read -r remote_path; do
            [[ -z "$remote_path" ]] && continue

            # Extract title from path: /root/jobs/<TITLE>/enhanced_Nx.mkv
            local title
            title=$(echo "$remote_path" | sed 's|.*/jobs/\([^/]*\)/.*|\1|')
            local filename
            filename=$(basename "$remote_path")
            local local_file="$dest/${title}_${filename}"

            if [[ -f "$local_file" ]]; then
                log "  Already downloaded: $title"
                continue
            fi

            log "  Downloading: $title ($filename)..."
            if vastai copy "$iid:$remote_path" "$local_file" 2>/dev/null; then
                log "  Done: $local_file"
                # Extract video ID from completed.txt on instance
                echo "$title" >> "$COMPLETED_FILE"
            else
                log "  WARNING: Failed to download $title from instance $iid"
            fi
        done
    done

    local count
    count=$(ls -1 "$dest"/*.mkv 2>/dev/null | wc -l || echo 0)
    log "Download complete. $count videos in $dest/"
}

cmd_destroy() {
    log "Destroying all vast.ai instances..."

    local instance_ids
    instance_ids=$(get_instance_ids)

    if [[ -z "$instance_ids" ]]; then
        log "No tracked instances to destroy."
        local running
        running=$(vastai show instances -q 2>/dev/null || true)
        if [[ -n "$running" ]]; then
            echo "Found untracked running instances:"
            vastai show instances 2>/dev/null
            echo ""
            read -r -p "Destroy all? (y/N) " confirm
            if [[ "$confirm" == "y" || "$confirm" == "Y" ]]; then
                for iid in $running; do
                    vastai destroy instance "$iid" 2>/dev/null && log "Destroyed $iid" || true
                done
            fi
        fi
        return
    fi

    for iid in $instance_ids; do
        log "Destroying instance $iid..."
        vastai destroy instance "$iid" 2>/dev/null && log "  Destroyed." || log "  Failed (already destroyed?)"
    done

    rm -f "$STATE_DIR"/instance_*.id
    log "All instances destroyed."
}

cmd_resume() {
    ensure_state_dir

    log "Checking existing instances..."

    local running
    running=$(get_running_instances)
    if [[ -z "$running" ]]; then
        log "No running instances found. Use './vast_batch.sh launch' to start."
        return
    fi

    echo "$running" | while IFS=$'\t' read -r iid gpu price host ssh_port status web_port label; do
        log "Instance $iid ($label): $status"
        if [[ -n "$web_port" && "$web_port" != "0" ]]; then
            log "  Status page: http://${host}:${web_port}"
        fi

        local completed
        completed=$(vastai execute "$iid" "cat /root/completed.txt 2>/dev/null | wc -l" 2>/dev/null || echo "?")
        log "  Completed: $completed videos"

        local status_line
        status_line=$(vastai execute "$iid" "tail -1 /root/enhance.log 2>/dev/null" 2>/dev/null || true)
        log "  Last log: $status_line"
    done
}

cmd_list() {
    echo "=== davaz.com Video List (226 videos) ==="
    echo ""
    printf "%-15s %-55s %8s %4s %5s\n" "VIDEO_ID" "TITLE" "DURATION" "DEF" "SCALE"
    echo "------------------------------------------------------------------------------------------------------"

    local total_sec=0
    while IFS=$'\t' read -r vid dur def scale title; do
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
        if [[ ${#display_title} -gt 53 ]]; then
            display_title="${display_title:0:50}..."
        fi
        printf "%-15s %-55s %8s %4s %4sx\n" "$vid" "$display_title" "$dur_str" "$def" "$scale"
        total_sec=$((total_sec + dur))
    done <<< "$VIDEO_DATA"

    echo "------------------------------------------------------------------------------------------------------"
    local th=$((total_sec / 3600))
    local tm=$(( (total_sec % 3600) / 60 ))
    echo "Total: $(get_video_count) videos, ${th}h ${tm}m of content"
    echo ""
    echo "HD: $(echo "$VIDEO_DATA" | grep -c $'\thd\t') videos (2x upscale)"
    echo "SD: $(echo "$VIDEO_DATA" | grep -c $'\tsd\t') videos (4x upscale)"
    echo ""
    echo "Job directories use movie titles, e.g.:"
    echo "  ~/jobs/064_S(T)INGING_BEAUTY_Kamchatka_-_RussianEsub/"
    echo "  ~/jobs/CAMBODIA_DUST_of_LIFE/"
}

# ---------- Main ----------

cmd_url() {
    local url="$1"
    local scale="${2:-4}"
    ensure_state_dir

    if [[ -z "$url" ]]; then
        echo "Usage: ./vast_batch.sh <youtube-url> [scale]"
        echo "  scale: 2 or 4 (default: 4)"
        exit 1
    fi

    # Extract video ID
    local vid
    vid=$(echo "$url" | sed -n 's/.*[?&]v=\([^&]*\).*/\1/p')
    if [[ -z "$vid" ]]; then
        vid=$(echo "$url" | sed -n 's|.*/\([^/?]*\).*|\1|p')
    fi
    if [[ -z "$vid" ]]; then
        echo "ERROR: Could not extract video ID from URL"
        exit 1
    fi

    # Get video info via yt-dlp
    log "Fetching video info..."
    local dur width raw_title
    dur=$(yt-dlp --print "%(duration)s" "$url" 2>/dev/null)
    width=$(yt-dlp --print "%(width)s" "$url" 2>/dev/null)
    raw_title=$(yt-dlp --print "%(title)s" "$url" 2>/dev/null)
    if [[ -z "$dur" || -z "$raw_title" ]]; then
        echo "ERROR: Could not fetch video info. Check the URL."
        exit 1
    fi
    dur="${dur%.*}"  # remove decimals
    width="${width:-0}"

    # Clean title for directory name
    local title
    title=$(echo "$raw_title" | sed 's/[^a-zA-Z0-9._-]/_/g' | sed 's/__*/_/g' | sed 's/^_//;s/_$//')

    # Determine definition and recommend scale
    local def="sd"
    if [[ "$width" -ge 1280 ]]; then
        def="hd"
        if [[ "$scale" -eq 4 ]]; then
            log "Video is HD (${width}px wide). Recommending 2x upscale."
            scale=2
        fi
    fi

    local dur_str
    local h=$((dur / 3600))
    local m=$(( (dur % 3600) / 60 ))
    if [[ $h -gt 0 ]]; then
        dur_str="${h}h ${m}m"
    else
        dur_str="${m}m"
    fi

    log "=== Single Video Enhancement ==="
    log "Title:    $raw_title"
    log "ID:       $vid"
    log "Duration: $dur_str ($def, ${width}px)"
    log "Scale:    ${scale}x"
    log "Job name: $title"
    log ""

    if [[ ! -f "$HOME/.config/vastai/vast_api_key" ]]; then
        echo "ERROR: No vast.ai API key found."
        echo "Run: vastai set api-key YOUR_API_KEY"
        exit 1
    fi

    # Check credit
    local balance
    balance=$(vastai show user --raw 2>/dev/null | python3 -c "import json,sys; d=json.load(sys.stdin); print(f'{d.get(\"balance\",0):.2f}')" 2>/dev/null || echo "?")
    log "vast.ai credit: \$$balance"

    # Write single-video assignment
    echo -e "${vid}\t${scale}\t${title}\t${dur}" > "$ASSIGNMENTS_DIR/instance_0.txt"

    # Generate onstart script
    local video_list
    video_list=$(cat "$ASSIGNMENTS_DIR/instance_0.txt")
    local onstart_file="$STATE_DIR/onstart_url.sh"
    generate_onstart_script "$video_list" > "$onstart_file"

    # Search for cheapest RTX 4090
    # Calculate disk needs based on video resolution and duration
    local needed_disk_gb=$DISK_GB
    if [[ -n "$width" && -n "$dur" ]]; then
        local height=$((width * 3 / 4))  # approximate
        local fps=25
        local total_frames=$((dur * fps))
        local input_gb=$((total_frames * width * height * 3 / 3 / 1073741824))
        local output_gb=$((total_frames * width * scale * height * scale * 3 / 3 / 1073741824))
        needed_disk_gb=$((input_gb + output_gb + 10))
        if [[ "$needed_disk_gb" -lt "$DISK_GB" ]]; then
            needed_disk_gb=$DISK_GB
        fi
        log "Estimated disk needed: ${needed_disk_gb}GB"
    fi

    log "Searching for cheapest $GPU_NAME with >=${needed_disk_gb}GB disk..."
    local offers
    offers=$(vastai search offers "gpu_name=$GPU_NAME num_gpus=$NUM_GPUS reliability>0.95 disk_space>=$needed_disk_gb inet_down>100 rented=False direct_port_count>=1" -o 'dph_total' --raw 2>/dev/null)

    local available
    available=$(echo "$offers" | python3 -c "import json,sys; d=json.load(sys.stdin); print(len(d))" 2>/dev/null || echo 0)

    if [[ "$available" -eq 0 ]]; then
        echo "No suitable RTX 4090 offers with >=${needed_disk_gb}GB disk found."
        echo "Try a smaller video or lower scale."
        exit 1
    fi

    local offer_id price gpu
    read -r offer_id price gpu <<< "$(echo "$offers" | python3 -c "
import json, sys
DISK_GB = $DISK_GB
data = json.load(sys.stdin)
for d in data:
    stor = d.get('storage_cost', 0) or 0
    d['real_dph'] = d.get('dph_base', d.get('dph_total', 0)) + stor * DISK_GB / 730
data.sort(key=lambda x: x.get('real_dph', 999))
d = data[0]
print(f\"{d['id']}\t{d.get('real_dph',0):.4f}\t{d.get('gpu_name','?')}\")
")"

    log "Selected: $gpu @ \$$price/hr (incl. storage for ${DISK_GB}GB)"

    # Estimate
    local est_seconds
    if [[ "$scale" -eq 4 ]]; then
        est_seconds=$((dur * 30))
    else
        est_seconds=$((dur * 15))
    fi
    local est_hours=$(echo "$est_seconds / 3600" | bc -l 2>/dev/null || echo "?")
    local est_cost=$(echo "$est_hours * $price" | bc -l 2>/dev/null || echo "?")
    log "Estimated: ~${est_hours%.*}h, ~\$${est_cost%.*}"
    log ""

    log "Creating instance..."
    local result
    result=$(vastai create instance "$offer_id" \
        --image "$DOCKER_IMAGE" \
        --disk "$needed_disk_gb" \
        --ssh \
        --direct \
        --env "-p ${STATUS_PORT}:${STATUS_PORT}" \
        --onstart "$onstart_file" \
        --label "davaz-url" \
        --raw 2>&1) || true

    local instance_id
    instance_id=$(echo "$result" | python3 -c "
import json, sys
try:
    data = json.load(sys.stdin)
    if isinstance(data, dict) and 'new_contract' in data:
        print(data['new_contract'])
    elif isinstance(data, dict) and 'id' in data:
        print(data['id'])
    else:
        print(data)
except:
    print(sys.stdin.read().strip())
" 2>/dev/null || echo "$result")

    if [[ -n "$instance_id" && "$instance_id" != "null" ]]; then
        echo "$instance_id" > "$STATE_DIR/instance_url.id"
        log "Instance created: ID=$instance_id"
    else
        log "ERROR: Failed to create instance: $result"
        exit 1
    fi

    echo ""
    log "Enhancement started!"
    log "  Video: $raw_title"
    log "  Scale: ${scale}x"
    log ""
    log "Next steps:"
    log "  1. ./vast_batch.sh status     — check progress + get dashboard URL"
    log "  2. Open dashboard URL          — watch live progress + compare frames"
    log "  3. ./vast_batch.sh download   — download the enhanced video when done"
    log "  4. ./vast_batch.sh destroy    — clean up instance"
}

case "${1:-help}" in
    http*|https*)
        cmd_url "$1" "${2:-4}"
        ;;
    test)
        cmd_test "${2:-}"
        ;;
    launch)
        cmd_launch "${2:-4}"
        ;;
    status)
        cmd_status
        ;;
    download)
        cmd_download "${2:-}"
        ;;
    destroy)
        cmd_destroy
        ;;
    resume)
        cmd_resume
        ;;
    list)
        cmd_list
        ;;
    help|--help|-h)
        echo "Usage: $0 <command> [options]"
        echo "       $0 <youtube-url> [scale]"
        echo ""
        echo "Commands:"
        echo "  <youtube-url> [2|4]  Enhance a single YouTube video (scale: 2 or 4, default: 4)"
        echo "  test [VIDEO_ID]   Launch 1 instance with 1 video to check quality first"
        echo "  launch [N]        Launch N instances (default: 4) and start processing all"
        echo "  status            Show progress + web status page URLs"
        echo "  download [DIR]    Download completed videos (default: ./enhanced_videos/)"
        echo "  destroy           Destroy all running instances"
        echo "  resume            Check and resume existing instances"
        echo "  list              List all 226 videos with titles and durations"
        echo ""
        echo "Each instance runs a web status page on port $STATUS_PORT showing"
        echo "per-video progress with title, status bar, and live log tail."
        echo ""
        echo "Workflow (recommended):"
        echo "  1. vastai set api-key YOUR_KEY    # one-time setup"
        echo "  2. ./vast_batch.sh test            # test with 1 video first"
        echo "  3. ./vast_batch.sh status          # monitor + check quality"
        echo "  4. ./vast_batch.sh download        # download test video"
        echo "  5. ./vast_batch.sh destroy          # destroy test instance"
        echo "  6. ./vast_batch.sh launch 4        # launch full batch if quality OK"
        echo "  7. ./vast_batch.sh download        # download all when done"
        echo "  8. ./vast_batch.sh destroy          # clean up"
        echo ""
        echo "Job directories use movie titles (not video IDs):"
        echo "  ~/jobs/064_S(T)INGING_BEAUTY_Kamchatka_-_RussianEsub/enhanced_2x.mkv"
        echo ""
        echo "Cost estimate (4x RTX 4090 @ \$0.34/hr):"
        echo "  ~\$490 total, ~15 days wall-clock time"
        ;;
    *)
        echo "Unknown command: $1"
        echo "Run '$0 help' for usage."
        exit 1
        ;;
esac
