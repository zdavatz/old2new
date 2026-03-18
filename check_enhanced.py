#!/usr/bin/env python3
"""
Check which Da Vaz videos don't have an Enhanced 4K version yet on YouTube.
Uses the YouTube Data API v3 to search by channel.
"""

import os
import sys
import json

# All 226 video IDs with metadata from the batch scripts
VIDEO_DATA = """
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
""".strip()


def parse_videos():
    """Parse embedded video data into list of dicts."""
    videos = []
    for line in VIDEO_DATA.split('\n'):
        line = line.strip()
        if not line:
            continue
        parts = line.split('\t')
        if len(parts) >= 5:
            videos.append({
                'id': parts[0],
                'duration': int(parts[1]),
                'definition': parts[2],
                'scale': int(parts[3]),
                'title': parts[4],
            })
    return videos


def get_youtube_service():
    """Authenticate with YouTube API."""
    from google_auth_oauthlib.flow import InstalledAppFlow
    from google.auth.transport.requests import Request
    from google.oauth2.credentials import Credentials
    from googleapiclient.discovery import build

    SCOPES = [
        "https://www.googleapis.com/auth/youtube.readonly",
    ]

    token_file = "youtube_token.json"
    client_secret = "client_secret.json"

    creds = None
    if os.path.exists(token_file):
        creds = Credentials.from_authorized_user_file(token_file, SCOPES)

    if not creds or not creds.valid:
        if creds and creds.expired and creds.refresh_token:
            creds.refresh(Request())
        else:
            flow = InstalledAppFlow.from_client_secrets_file(client_secret, SCOPES)
            creds = flow.run_local_server(port=0)
        with open(token_file, "w") as f:
            f.write(creds.to_json())

    return build("youtube", "v3", credentials=creds)


def get_channel_videos(youtube):
    """Get all videos from the authenticated user's channel, find Enhanced 4K ones."""
    # First get the channel ID
    channels = youtube.channels().list(part="id,snippet", mine=True).execute()
    if not channels.get("items"):
        print("ERROR: No channel found for authenticated user")
        sys.exit(1)

    channel_id = channels["items"][0]["id"]
    channel_name = channels["items"][0]["snippet"]["title"]
    print(f"Channel: {channel_name} ({channel_id})")

    # Search for all "Enhanced 4K" videos on this channel
    enhanced_titles = set()
    enhanced_videos = []
    page_token = None

    print("Searching for Enhanced 4K videos on channel...")
    while True:
        results = youtube.search().list(
            part="snippet",
            channelId=channel_id,
            q="Enhanced 4K",
            type="video",
            maxResults=50,
            pageToken=page_token,
        ).execute()

        for item in results.get("items", []):
            title = item["snippet"]["title"]
            if "(Enhanced" in title:
                enhanced_titles.add(title)
                enhanced_videos.append({
                    'id': item['id']['videoId'],
                    'title': title,
                })

        page_token = results.get("nextPageToken")
        if not page_token:
            break

    print(f"Found {len(enhanced_videos)} Enhanced 4K videos on channel\n")
    return enhanced_videos, enhanced_titles


def get_video_resolutions(youtube, video_ids):
    """Fetch actual resolution (width x height) for a batch of video IDs."""
    resolutions = {}
    # YouTube API allows max 50 IDs per request
    for i in range(0, len(video_ids), 50):
        batch = video_ids[i:i+50]
        response = youtube.videos().list(
            part="contentDetails,snippet",
            id=",".join(batch),
        ).execute()

        for item in response.get("items", []):
            vid = item["id"]
            # contentDetails has definition (hd/sd) but not exact resolution
            # We need to check via streams - but YouTube API v3 doesn't expose
            # exact resolution in videos.list. We'll use definition + known patterns.
            definition = item["contentDetails"].get("definition", "sd")
            title = item["snippet"]["title"]
            resolutions[vid] = {
                'definition': definition,
                'title': title,
            }

    return resolutions


def main():
    all_videos = parse_videos()
    print(f"Total videos in collection: {len(all_videos)}")
    print(f"  HD videos: {sum(1 for v in all_videos if v['definition'] == 'hd')}")
    print(f"  SD videos: {sum(1 for v in all_videos if v['definition'] == 'sd')}")
    print()

    youtube = get_youtube_service()
    enhanced_videos, enhanced_titles = get_channel_videos(youtube)

    # Match enhanced titles back to originals
    # Enhanced title = original title + " (Enhanced 4K)"
    # But titles on YouTube may differ from our metadata titles (underscores vs spaces etc)
    # So we also check by fetching each original and seeing if Enhanced version exists

    # Build a set of original video IDs that have been enhanced
    # Strategy: for each enhanced title, strip " (Enhanced 4K)" and match
    enhanced_originals = set()
    for et in enhanced_titles:
        # Remove suffix
        base = et.replace(" (Enhanced 4K)", "").replace(" (Enhanced 2K)", "").strip()
        enhanced_originals.add(base.lower())

    # Now check each original video - fetch actual titles from YouTube
    print("Fetching original video details from YouTube...")
    all_ids = [v['id'] for v in all_videos]
    resolutions = get_video_resolutions(youtube, all_ids)

    # Find videos without enhanced versions
    missing = []
    found = []
    not_on_youtube = []

    for v in all_videos:
        if v['id'] not in resolutions:
            not_on_youtube.append(v)
            continue

        yt_title = resolutions[v['id']]['title'].lower()
        yt_def = resolutions[v['id']]['definition']

        # Check if enhanced version exists
        has_enhanced = yt_title in enhanced_originals
        if not has_enhanced:
            # Also try with the title from our metadata
            meta_title = v['title'].replace('_', ' ').lower()
            has_enhanced = meta_title in enhanced_originals

        if has_enhanced:
            found.append(v)
        else:
            v['yt_title'] = resolutions[v['id']]['title']
            v['yt_definition'] = yt_def
            missing.append(v)

    print(f"\n{'='*80}")
    print(f"RESULTS")
    print(f"{'='*80}")
    print(f"Already enhanced:      {len(found)}")
    print(f"NOT enhanced yet:      {len(missing)}")
    print(f"Not found on YouTube:  {len(not_on_youtube)}")
    print()

    if not_on_youtube:
        print(f"--- Videos NOT found on YouTube ({len(not_on_youtube)}) ---")
        for v in not_on_youtube:
            print(f"  {v['id']}  {v['title']}")
        print()

    # Sort missing by duration descending
    missing.sort(key=lambda x: x['duration'], reverse=True)

    # Separate by GPU requirement
    sd_videos = [v for v in missing if v['definition'] == 'sd']
    hd_videos = [v for v in missing if v['definition'] == 'hd']

    print(f"--- Videos WITHOUT Enhanced 4K ({len(missing)}) ---")
    print(f"  SD (need RTX 4090, 4x upscale): {len(sd_videos)}")
    print(f"  HD (need RTX 5090/A100, 2x upscale): {len(hd_videos)}")
    print()

    # Print all missing videos
    print(f"{'ID':<15} {'Def':>3} {'Scale':>5} {'Duration':>8} {'GPU':>12}  Title")
    print(f"{'-'*15} {'-'*3} {'-'*5} {'-'*8} {'-'*12}  {'-'*40}")
    for v in missing:
        dur_min = v['duration'] / 60
        gpu = "RTX 5090" if v['definition'] == 'hd' else "RTX 4090"
        print(f"{v['id']:<15} {v['definition']:>3} {v['scale']:>5}x {dur_min:>7.1f}m {gpu:>12}  {v['yt_title']}")

    # Suggest a batch of ~20 similar videos for upscaling
    print(f"\n{'='*80}")
    print(f"SUGGESTED BATCH OF ~20 VIDEOS FOR UPSCALING")
    print(f"{'='*80}")

    # Group by GPU type and pick ~20 similar ones
    # Prefer SD videos first (RTX 4090 is cheaper and more available)
    if len(sd_videos) >= 20:
        # Pick 20 SD videos with similar duration for efficient batching
        # Sort by duration and pick a contiguous block of medium-length ones
        sd_sorted = sorted(sd_videos, key=lambda x: x['duration'])
        # Pick from the middle for balanced batch
        mid = max(0, len(sd_sorted) // 2 - 10)
        batch = sd_sorted[mid:mid+20]
        gpu_type = "RTX 4090"
        scale = "4x"
    elif len(sd_videos) > 0:
        batch = sd_videos[:20]
        gpu_type = "RTX 4090"
        scale = "4x"
    elif len(hd_videos) >= 20:
        hd_sorted = sorted(hd_videos, key=lambda x: x['duration'])
        mid = max(0, len(hd_sorted) // 2 - 10)
        batch = hd_sorted[mid:mid+20]
        gpu_type = "RTX 5090 / A100 80GB"
        scale = "2x"
    else:
        batch = hd_videos[:20]
        gpu_type = "RTX 5090 / A100 80GB"
        scale = "2x"

    batch.sort(key=lambda x: x['duration'], reverse=True)
    total_duration = sum(v['duration'] for v in batch)
    total_frames = sum(v['duration'] * 25 for v in batch)  # ~25 fps estimate

    print(f"\nGPU: {gpu_type}")
    print(f"Scale: {scale}")
    print(f"Videos: {len(batch)}")
    print(f"Total duration: {total_duration/3600:.1f} hours")
    print(f"Est. total frames: {total_frames:,}")
    print()

    # Estimate processing time
    fps = 2.6 if gpu_type == "RTX 4090" else 2.0
    est_hours = total_frames / fps / 3600
    cost_per_hr = 0.41 if "4090" in gpu_type else 0.80
    est_cost = est_hours * cost_per_hr

    print(f"Est. processing time per GPU: {est_hours:.1f} hours")
    print(f"Est. cost per GPU: ${est_cost:.2f}")
    print(f"With 4 parallel instances: {est_hours/4:.1f} hours, ${est_cost:.2f} total")
    print()

    print(f"{'#':>2} {'ID':<15} {'Duration':>8} {'Def':>3}  Title")
    print(f"{'-'*2} {'-'*15} {'-'*8} {'-'*3}  {'-'*50}")
    for i, v in enumerate(batch, 1):
        dur_min = v['duration'] / 60
        print(f"{i:>2} {v['id']:<15} {dur_min:>7.1f}m {v['definition']:>3}  {v['yt_title']}")

    # Save results to JSON for further use
    output = {
        'total_videos': len(all_videos),
        'already_enhanced': len(found),
        'not_enhanced': len(missing),
        'not_on_youtube': len(not_on_youtube),
        'missing_sd': [{'id': v['id'], 'title': v.get('yt_title', v['title']), 'duration': v['duration'], 'scale': v['scale']} for v in sd_videos],
        'missing_hd': [{'id': v['id'], 'title': v.get('yt_title', v['title']), 'duration': v['duration'], 'scale': v['scale']} for v in hd_videos],
        'suggested_batch': [{'id': v['id'], 'title': v.get('yt_title', v['title']), 'duration': v['duration'], 'definition': v['definition'], 'scale': v['scale']} for v in batch],
        'batch_gpu': gpu_type,
        'batch_scale': scale,
    }

    with open('enhanced_status.json', 'w') as f:
        json.dump(output, f, indent=2)
    print(f"\nResults saved to enhanced_status.json")


if __name__ == "__main__":
    main()
