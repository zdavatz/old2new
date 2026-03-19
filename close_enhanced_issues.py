#!/usr/bin/env python3
"""
Close GitHub issues for videos that have been uploaded as "Enhanced 4K" to YouTube.

Workflow:
1. Query YouTube API for all "(Enhanced 4K)" videos on the Da Vaz channel
2. Match each enhanced video back to its original video ID
3. Find open GitHub issues in zdavatz/old2new that match
4. Close them with a comment linking to the new Enhanced 4K video

Usage:
    python3 close_enhanced_issues.py          # dry-run (show what would be closed)
    python3 close_enhanced_issues.py --close   # actually close the issues

Requires:
    - youtube_token.json + client_secret.json (YouTube OAuth2)
    - gh CLI authenticated (for GitHub issue operations)
"""

import os
import sys
import json
import subprocess
import re

# All 226 original video IDs mapped to titles (from batch scripts)
# Format: youtube_id -> title
VIDEO_DATA = """
o_nM2N-03UI	064_S(T)INGING_BEAUTY_Kamchatka_-_RussianEsub
aUg2dv4XXgM	050a_BORN_to_MOVE_X-treams_X-dreams_-_Kazakhstan
mgUOHubnEC8	CAMBODIA_DUST_of_LIFE
l8szkLe2eiM	077c_Gruz_Koztarsasag_Ahol_Isten_Leszallt_-_Hsub
cWGbmkCvGHA	077b_Republik_Georgien_FUSSTRITTE_GOTTES_-_RGAODsub
q_UgL0Pbet8	077a_Republic_of_Georgia_WHERE_GOD_LANDED_-_RGAOEsub
NyUEAixkcfQ	DPRK_Hero_to_Zero_eyes_wide_-_mouth_shut
tljAVZCj6lw	BLUEPRINTS_of_LIFE
fuNkC-JNzEc	RUSSIA_TRAPPED_REALITY
tkCxiE1Wlrw	KAZAKHSTAN_ETERNAL_HOME
N_Ui88q-gy8	072a_Iran_CHADOR_CONDOM_COFFEESHOP_-_FarsiEsub
opavvOpVpUM	072b_Iran_KOPFTUCH_PARISER_KAFFEEHAUS_-_FarsiDsub
v1YJC8dMaas	JAPAN_High_School_Girls_-_SHIBUYA_-_1712
9lZDEOnRgSU	054b_HUMUS_for_HAMAS_-_ArabicHsub
hH4mTkFUKdg	FILMTIME_MTV_m2_Hungarian_Television
G9Whw4gJCeY	054a_HUMUS_for_HAMAS_Gaza_Strip_-_ArabicEsub
WwHJW39JqPk	050c_BORN_to_MOVE_X-treams_X-dreams_-_KHsub
R2zxpDOmjfA	CHINA_CCTV_-_PORTRAIT_of_JURG_DA_VAZ
Uv5kGuyJSbc	Humus_for_Hamas_subtitles_in_FARSI_24.09.2010
Us1tVOObESU	022b_21st_Century_China_4539_feb07
u9LHBYxyj5w	VERGANGENHEIT_als_VERMACHTNIS_work_in_progress_1.0
Q9VKZeHaUIc	BHUTAN_LIVE
2udCsqGRnyg	China_CCTV_-_I_TOOLS_-_EYE_TOOLS_-_a_portrait_of_Jurg_Da_Vaz
rXUD-jxOCuM	051a_MISHA_goes_to_SCHOOL_-_REsub
8wqZivWVLZs	BUDAPEST_diary
SHFqywMgx1Q	007b_BUDAPESTEN_NAPLO_-_H
c62HSWqoxKo	RUSSIA_REALtime
Rmjega-OZQg	INDIA_O_LUCKY_CALCUTTA
X3J2WOb0FyI	030a_ALICE_in_WONDERLAND_-_E
4_gaU85Zzog	075a_MAOs_BARBERSHOP_-_chineseEsub
dOF5NTLfmn0	A_TESTAMENTUM_Yevgenyi_Burak
v27m8UT4w0M	074c_ZAVESHCHANIE_EVGENIYU_BURAKU_-_R
-eTyUFV-KB8	DAS_TESTAMENT_-_Yevgenyi_Burak
t6YvdmcGdo8	049b_Liebe_Anna_-_D
KHRHsyGIRlI	049a_Liebe_Anna_-_E
UExLHdyGfwA	049c_Liebe_Anna_-_H
XtzTRGmGWuM	fucking_good_KAMCHATKA_kurva_jo_(Hungarian_Edition)
e1e-_J0PzTY	Museum_Rundgang_Werni_2024
rxHoEF73O5k	fucking_good_KAMCHATKA_Ballett_fur_Zwei_Fischdiebe_(Deutsche_Edition)
Ydkc8oZzHBY	fucking_good_KAMCHATKA_Ballet_for_Two_Poachers_(English_Edition)
MpZicz5Nkrg	ISRAEL_ITZHAK_FREY_FIA_(Hungarian_Edition)
5Wu0PCEahsg	017_SIKKIM_Stories
roeVmHWKobs	RUSSIA_OREG_TO_(Hungarian_Edition)
BXdKRjSVsvg	DPRK_Dongan_-_Pyongyang_uncensored_footage
T0Z6t9zG0rw	TRAPPED_REALITY_-_Short_Stories_from_Moscow
ZsfwCuMHFVw	010b_SUITE_702_-_H
Y3Zb55v7sM4	RUSSIA_SUITE_702
WBsY8GGMOBE	001a_THE_OTHER_EYE_-_E
KMclVtn2aoc	Republic_of_GEORGIA_KMARA_--_ENOUGH_THE_ROSE_REVOLUTION
0tapt-cyoSY	KAMCHATKA_BEAR_and_FISH_(English_Edition)
Hqa_G12v0Bw	004_WORKS_in_PROGRESS
acjxyO710lw	023a_RAISING_CHINA_Interview_H-E
j1J5t163asA	KAZAKHSTAN_BALANCE_of_LIFE_BABUSHKA_MASHA_(English_Edition)
vVJAdgh-yxo	KAZAKHSTAN_EGYENSULYBAN_BABUSHKA_MASHA_(Hungarian_Edition)
M3D3Gxo4AEQ	013_PIG_OPERA
9Rz8jpSUmPM	KAMCHATKA_LOST_in_TRANSITION_(English_Edition)
w2zIO_8S3Ek	Republic_of_GEORGIA_drunk_GOD
8SvgnUHDdTU	008_PICK_PEN
Kv7e-5ii-Ew	RUSSIA_DEAD_END
0DNzxYaPVPs	KAZAKHSTAN_ARAL_SEA_-_Moments_for_Monuments
4bLA0adDPaI	Palestine_HAIDER_ABDELSHAFI_-_last_interview
AtuAAkLGRPo	RettungsMarsch_1913_-_saving_20_seconds_(Long_Version)_2130
cMvVKrXIN9Y	024_CITY_in_MAKING
Z3JLO-Vpk5U	ARCHITECTURE_of_a_TRIAL
bnbaapXPOjg	085_c_Maasai_People
lA-HJkXDE2A	GAZA_35_Million_Dollars_in_a_Suitcase_Gaza_in_Pain
NKXxfXK8qbI	HONGKONG_MORPHOPOLIS
6hUUJNMZKDw	KAMCHATKA_TOLBACHIK_1975-2005
4_-BxRL1vFs	005_pf-ERDE_am_HIMMEL
1W9mjMtxVSw	015_Last_Night_I_was_Two_Cats
i8XzcDXpBoY	CALL_OF_THE_SNOUT
GSiput8wrVE	016_CABAGGE_TALES
Da3-JcXMnzQ	PIGstile
PPCRMgkcBFc	PIG_OPERA
FnlKDiLSmA0	011b_BEZARVA_Moscow_-_Russian_Hungarian_sub
W8aE4bWqX-U	011a_BUTIRSKAYA_PRISON_Moscow_-_E
JXir0H9XPzY	009_ChickenPick
-x_aIkSrXFw	DPRK_Pyongyang_Metro
Oy6C9xa-IkQ	86c_Baboons_out_of_Kenya
PVSGvKb2pzU	DPRK_Overland_from_Panmunjon_to_Pyongyang
aOn12f4HN4E	083e_DPRK_haircut_in_a_cooperative_state_farm_-_Wonsan
41GCHnsBV8Y	TIBET_Everest_Base_Camp
m4j94BSQMp4	GIRLS_CHOIR_DPRK
0RRuKv3u7dU	DPRK_GAYAGEUM_-_concert_zither
_dxcOch0CrU	INTERVIEW_FOKUSZ_RTL_CLUB
AkAtP0oKCMs	ACCORDION_LESSON_DPRK
AjiFJbsRYyg	085a_50_tons_of_wild_life_-_Kenya
4QqadqB5ZSs	PIANO_PRACTISE_DPRK
6L-z22_WnvA	Fly_over_Kamchatka
4sgXo0I4uTI	DPRK_Swiss_Yodeling_over_Pyongyang
kIIFdZrQuTQ	Hairdresser_in_Georgia
6JCZA5BMpg0	DPRK_37th_floor_-_Pyongyang
lCpwATzkfMA	DRAWING_PRACTISE_DPRK
rNtr5Y7Yaok	DPRK_Guns_and_Cherry_Blossoms
pdya9ZzlGhg	DPRK_DRAMA_CLASS
y81sU3NL3Fo	DPRK_TRADITIONAL_DANCE
sIK1U7gS-tw	BRUSH_PAINTING_DPRK
td5h1Gcx0gg	How_do_you_do_in_DPRK
wtn4-4bQKsk	Itzhak_Frey_gondolata_a_Felvidekrol
DR9rSeRX8ug	054ca_zwolf_sekunden_zensur_HUMUS_for_HAMAS
d6ph7n4k35Y	009_ChickenPick
aefe1fn7Kf0	050b_BORN_to_MOVE_-_RussianEsub
OKBfMOq1cEk	045b_SZIBERIA_TE_VAGY_A_HATAROM_-_RHsub
GRyA3D7VSAA	045a_SIBERIA_YOU_ARE_MY_LIMIT_-_REsub
eGQk9DFJZ2E	059a_KRAPIVNAJA_-_LOST_in_TRANSITION_-_REsub
mSQYaAkiTqo	059b_KRAPIVNAJA_-_LOST_in_TRANSITION_-_RHsub
nXFQ4DfXlCk	060a_LAZAC_ES_MACKO_PARADICSOM_-_REsub
Sz8L2nfRxoI	060b_LAZAC_ES_MACKO_PARADICSOM_-_RHsub
DNPZ2W5O4jM	061a_TOLBACHIK_-_Ludmillas_Volcano_-_E
QnIGhABaHXA	061b_TOLBACHIK_-_Ludmilla_Vulkanja_-_RHsub
pE_5SL0fJ78	046a_BALANCE_of_LIFE_Babushka_Masha_-_E
ghmkTELT9Xs	046b_EGYENSULYBAN_Babushka_Masha_-_H
TrNV3f8Tqo8	053a_JASMINE_and_OLIVES
iWGCGsUU4Cg	053b_JAZMIN_es_OLIVA_-_AHsub
DFIhHu34JL8	070a_Republic_of_GEORGIA_KMARA_-_ENOUGH_-_THE_ROSE_REVOLUTION
0Q_g9FSnJ5g	070b_KMARA_-_GENUG_ROSEN-REVOLUTION_-_GRD
5WyxX4gSXD0	070c_KMARA_-_ELEG_ROZSAS_FORRADALOM
l_pIeJmYr2g	055_Syria_Meat_-_eat_meat_-_meet_A_SACRIFICE
L8S_C78Abtk	068_SEX_on_the_STEPS_-_Yellow_Mountains
SqCz9S21S44	042_Behind_the_Curtain_-_Interview_HE
6LCYqXkIBig	038a_YOUNG_WOLF_Smolensk_-_RG_Esub
FdAMaDJ-5C8	038b_IFJU_FARKAS_Smolensk_-_H
7eSbhMwPwbk	081a_ZENG_FANZHI_meat_and_mask_-_ChineseEsub
JBvDnnRpvUw	081b_ZENG_FANZHI_EMBER_es_FENEDAD_-_ChineseHsub
rX4ADnOa3G4	082a_Syria_DEIR_ez-ZOR_CAMELS_for_BABIES_-_ArabicEsub
hGpmh9M3w4U	082b_Syria_Teveket_Gyerekekert_-_ArabicHsub
P3FvKpKl5SM	079a_Iran_HANDSHAKE_--_sometimes_KISSES_-_FarsiEsub
dX7HYP5rTp4	079b_Iran_KEZFOGAS_--_es_neha_csok_-_FarsiHsub
zNMJhKaVSLI	033_The_Takami_Family_Tokyo
_JuCTVfN0P8	078_The_FURNITURE_in_AFGHANISTAN_-_AfghanRussianEnglish
OiKSKP4e3RU	074b_DAS_TESTAMENT_Yevgenyi_Burak_-_D
BNe2xVvOtP4	074a_A_TESTAMENT_to_Yevgenyi_Burak_-_E
dh9_5M5LuCA	039a_Kazakh_Television_Interview_-_E
SiCo8Rh1ZFE	039b_Kazakh_Television_Interview_-_R
zVcHRn6U5Ds	073_WANG_GUANGYI_-_Communism_Pops_-_C
L5hPjL9PoCk	080_Zhou_Tie_Hai_MR._CAMEL_-_ChineseEnglish
OYVtWJYj58c	044_Book_of_Eyes_classic.
d_NeThR5Lsc	006_MTV2_Window_on_Europe_--_Europa_Ablak
I5nEcKz1crU	052a_Itzhak_Frey_Sohn_-_D
rLRyWOTCLJc	052b_Itzhak_Frey_Son_-_DEsub
C5v-bPpRFno	052d_Itzhak_Frey_Malchik_-_Russian_sub
jJGBkCd7t2s	052af_David_Frey_Kindheitserinnerungen
V5IG7XJaM_4	052ab_ITZHAK_FREY_Gedanken_zur_Grundung_des_Slowakischen_Staates_-_D
KP12RkZ0vOE	052ac_ITZHAK_FREY_Gedanken_zum_Antisemitismus_in_der_Schweiz_-_D
Fy3YPnlKpLY	052ad_ITZHAK_FREY_Gedanken_zu_Gebildeten_Menschen_Erdbeben_FC_Barcelona_-_D
J42Hm1gVt_U	untitled
hFL_Ct-qBRI	untitled
Yz_Sy4y26FQ	090_Heavenly_Private_Papamobil
JwxQhZMPfsY	DPRK_KIM_JONG_IL_moviemaker_-_all_about_love
v1T5-a3GMWE	067_SOMETHING_to_SAY_to_the_WORLD_-_Bests_from_Lis_out_of_Kenya
kD_kCmNGw6o	089_Sberbank_Kyiv
nSZLH8Cmv-U	094_Welcome_to_Ukraine
aQj2v_0ORFE	084_South_Korea_Penis_Parc_-_KoreanEsub
ufQlWI4G_ks	092_Tatra_Tram_in_Kyiv_Ukraine
VIZCqNqrctA	063_HARD_TALK_Biene_und_Mensch_-_D
5eaJ3zzrE6o	091_Dinner_at_Natasha_and_Petro
1wFQ_dABu9c	066_The_BEATLES_in_GEORGIA_-_GeorgianEsub
I5xz4elaSYk	088_Ukraines_Maidan_Kyiv
JU0-e2iG3zY	093_Karpfen_and_Mirage_Winniza_Ukraine
BEiQBaAq0IQ	085_DEER_VISION_-_Kenya
cXhj5-B0lPk	Republic_of_GEORGIA_TeaTime_in_TBILISI
J1VDYUbFDuA	MACH_aus_dem_VERGANGENEN_was_Du_KANNST
9JVNHyZBc2E	Suchen_Schnuffeln_Scharren_Zupacken
PkYE4nHDAx0	FUNdaziun_DA_VAZ_mumaints_dal_temp
C2G3q1RfL1w	Fundaziun_Da_Vaz_Ein_Einblick
VVfB_hA9MXU	TIBET_Sandpainting_Mandalas_Kumbum_Monastery
dVthWB2j52o	TIBET_till_mill
DWU7U9o14gc	TIBET_LHASA_Barkhor_Square_Potala_Palace
Mc8qHdQcPKo	TIBET_Rongbuk_Monastery
G44qQd9UPfY	TIBET_Sera_Monastery_Debating_Monks
ipmC1mfp9OA	030b_ALICE_in_WONDERLAND_-_H
4vj9O5E3Wz0	007a_BUDAPESTER_TAGEBUCH_-_D
5zyEfRXSrBk	030c_ALICE_in_WONDERLAND_-_German
SkRnCqyYgIg	At_natives_deep_inside_Kamchatka
X2eDe13cKoY	049d_Kedves_Anna_-_H
gD7Z2Ps8cfI	FISH_n_PISS
U1BFsP4Gxjg	UNEXPECTEDLY_SIGNIFICANT
7YwF2LpKBWw	THE_MESSAGE_OF_COLOR
Y_6mYZz1gPs	MAMA_TALK
3CXL5wFKiJY	GRILLED_PIG
J_0oGfXnbks	Whats_your_name
YpGTSHcasSk	PHILOMENA
qhYP_FaJGDo	FATHER_SAID
xV4C0Kqh4qI	1997_HAND_OVER
C98dqmBQHds	paphlipffpap-philipffppflapilphfefe
rNm55j6IzB0	REFLECTION
adJz_d5xO3A	WILL_TO_SURVIVE
M6kqS4L-nO8	PRESENT
wR9hSKMDCjc	CHICKEN
C3lMz1FCNaQ	oo
qfYxhJ5aHHY	hard_BEAT
n7A8JrmH2b0	WUNDERBLOCK
v2fxOIcPaAw	HELLO_HELLO
4n0VccxPuFQ	EVERYHANDIs_different
OQJBA3sUf_k	EMOTIONS
lKm5kHKjKHU	MY_STUDENTS
PiJ2LQTvUdI	process_isis_process
0mXpEGHdG_k	drip_drip
uVy3j80cLl4	the_OTHER_EYE
qSzGqnMiZ10	CABBAGE_NETWORK
W_oHTrpocBY	CUTTING-EDGE
pEjWZX8p_Bc	CROSS_BORDERS
Y43LuIECqzk	MATTER_OF_CHANCE
0xYdFdwCPtU	PICK_AND_PEN
ZAl51JiAVL4	roadside_BHUTAN
B8KnD0LFWgE	Bei_meiner_Ehre
3BgR_dOXzX4	WHYWHYWHY
fVo0JfHZI3U	nur_fur_DOKUMENT
Xwn0TZXxmkQ	CHICKENWORLD
bQ-1qWn0Y3Y	enter_BHUTAN
S7LYIXPqnVU	untitled
b8Mu-dZWpjA	DREAM_WORLD
Sh3T9u5EoG0	Its_a_Family_Game
_NnQ1rG6TY8	HEY_HEY
SqOm3EUxEMY	WECANNOTIMAGINE
XnQtQcAeEhg	Itzhak_Frei_working_in_his_own_bakery_in_Mea_Shearim_Jerusalem
-4YZLsDXKh4	Where_Where_HERE
lzVQk98YIZE	kidsKIDSkidsKIDSkids
_Br8-bsDXac	for_DISCOVERY
GmBG_-CxT50	DONT_LOOK_HERE
"""


def get_youtube_service():
    """Authenticate with YouTube API."""
    from google_auth_oauthlib.flow import InstalledAppFlow
    from google.auth.transport.requests import Request
    from google.oauth2.credentials import Credentials
    from googleapiclient.discovery import build

    SCOPES = ["https://www.googleapis.com/auth/youtube.readonly"]
    token_file = os.path.join(os.path.dirname(__file__), "youtube_token.json")
    client_secret = os.path.join(os.path.dirname(__file__), "client_secret.json")

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


def get_enhanced_videos(youtube):
    """Find all '(Enhanced 4K)' videos on the channel."""
    channels = youtube.channels().list(part="id,snippet", mine=True).execute()
    if not channels.get("items"):
        print("ERROR: No channel found")
        sys.exit(1)

    channel_id = channels["items"][0]["id"]
    channel_name = channels["items"][0]["snippet"]["title"]
    print(f"Channel: {channel_name} ({channel_id})")

    enhanced = []
    page_token = None
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
                enhanced.append({
                    "video_id": item["id"]["videoId"],
                    "title": title,
                    "published": item["snippet"]["publishedAt"],
                })

        page_token = results.get("nextPageToken")
        if not page_token:
            break

    print(f"Found {len(enhanced)} Enhanced 4K videos on YouTube\n")
    return enhanced


def build_original_id_map():
    """Map original video IDs to their titles (underscored and display)."""
    id_to_title = {}
    for line in VIDEO_DATA.strip().split("\n"):
        line = line.strip()
        if not line:
            continue
        parts = line.split("\t")
        if len(parts) >= 2:
            vid_id = parts[0].strip()
            title = parts[1].strip()
            id_to_title[vid_id] = title
    return id_to_title


def normalize(s):
    """Normalize a title for fuzzy matching."""
    import html
    s = html.unescape(s)  # &#39; -> '
    s = s.lower()
    # Remove common suffixes/prefixes
    s = re.sub(r'\(enhanced\s*4k\)', '', s)
    s = re.sub(r'\[processing\]', '', s)
    s = re.sub(r'\(\d+x upscale\)', '', s)
    # Remove emoji (Unicode blocks: emoticons, symbols, etc.)
    s = re.sub(r'[\U0001F000-\U0001FFFF]', '', s)
    # Normalize punctuation and whitespace
    s = re.sub(r'[_\-–—/\\,.:;!?\'\"&()%]', ' ', s)
    s = re.sub(r'\s+', ' ', s).strip()
    return s


def match_enhanced_to_originals(enhanced_videos, id_to_title):
    """Match enhanced YouTube videos back to original video IDs."""
    # Build reverse map: normalized title -> original video ID
    title_to_id = {}
    for vid_id, title in id_to_title.items():
        norm = normalize(title)
        title_to_id[norm] = vid_id

    matched = {}  # original_id -> enhanced_video_info
    unmatched = []

    for ev in enhanced_videos:
        enhanced_title = ev["title"]
        norm_enhanced = normalize(enhanced_title)

        # Try exact normalized match
        best_match = None
        best_score = 0
        for norm_orig, orig_id in title_to_id.items():
            # Check if one contains the other
            if norm_orig == norm_enhanced:
                best_match = orig_id
                best_score = 100
                break
            elif norm_orig in norm_enhanced or norm_enhanced in norm_orig:
                score = len(norm_orig) / max(len(norm_enhanced), 1) * 100
                if score > best_score:
                    best_score = score
                    best_match = orig_id

        if best_match and best_score > 50:
            matched[best_match] = ev
        else:
            unmatched.append(ev)

    return matched, unmatched


def get_open_issues():
    """Get all open issues from zdavatz/old2new via gh CLI."""
    result = subprocess.run(
        ["gh", "issue", "list", "--repo", "zdavatz/old2new",
         "--state", "open", "--json", "number,title", "--limit", "300"],
        capture_output=True, text=True
    )
    if result.returncode != 0:
        print(f"ERROR: gh issue list failed: {result.stderr}")
        sys.exit(1)
    return json.loads(result.stdout)


def match_issues_to_enhanced(issues, matched_originals, id_to_title, all_enhanced):
    """Find which open issues correspond to enhanced videos."""
    closeable = []
    already_matched = set()

    # Method 1: Match via matched_originals (original ID -> enhanced)
    for issue in issues:
        issue_title = issue["title"]
        norm_issue = normalize(issue_title)

        for orig_id, enhanced_info in matched_originals.items():
            orig_title = id_to_title.get(orig_id, "")
            norm_orig = normalize(orig_title)

            if norm_orig and (norm_orig in norm_issue or norm_issue in norm_orig):
                closeable.append({
                    "issue_number": issue["number"],
                    "issue_title": issue["title"],
                    "enhanced_id": enhanced_info["video_id"],
                    "enhanced_title": enhanced_info["title"],
                })
                already_matched.add(issue["number"])
                break

    # Method 2: Direct match — compare enhanced YouTube title with issue title
    for issue in issues:
        if issue["number"] in already_matched:
            continue
        norm_issue = normalize(issue["title"])
        for ev in all_enhanced:
            norm_enhanced = normalize(ev["title"])
            if norm_enhanced and norm_issue and len(norm_issue) > 3:
                if norm_enhanced in norm_issue or norm_issue in norm_enhanced:
                    closeable.append({
                        "issue_number": issue["number"],
                        "issue_title": issue["title"],
                        "enhanced_id": ev["video_id"],
                        "enhanced_title": ev["title"],
                    })
                    already_matched.add(issue["number"])
                    break

    return closeable


def close_issues(closeable, dry_run=True):
    """Close GitHub issues with a comment linking to the Enhanced 4K video."""
    for item in closeable:
        num = item["issue_number"]
        enhanced_url = f"https://www.youtube.com/watch?v={item['enhanced_id']}"
        comment = f"Enhanced 4K version uploaded: [{item['enhanced_title']}]({enhanced_url})"

        if dry_run:
            print(f"  [DRY-RUN] Would close #{num}: {item['issue_title']}")
            print(f"            → {enhanced_url}")
        else:
            result = subprocess.run(
                ["gh", "issue", "close", str(num),
                 "--repo", "zdavatz/old2new",
                 "--comment", comment],
                capture_output=True, text=True
            )
            if result.returncode == 0:
                print(f"  Closed #{num}: {item['issue_title']}")
            else:
                print(f"  FAILED #{num}: {result.stderr.strip()}")


def main():
    do_close = "--close" in sys.argv

    print("=== Close Enhanced Issues ===\n")

    # Step 1: Get enhanced videos from YouTube
    youtube = get_youtube_service()
    enhanced = get_enhanced_videos(youtube)

    # Step 2: Match to original video IDs
    id_to_title = build_original_id_map()
    matched, unmatched = match_enhanced_to_originals(enhanced, id_to_title)
    print(f"Matched {len(matched)} enhanced videos to originals")
    if unmatched:
        print(f"Unmatched: {len(unmatched)}")
        for u in unmatched:
            print(f"  ? {u['title']}")
    print()

    # Step 3: Find open GitHub issues to close
    issues = get_open_issues()
    print(f"Open GitHub issues: {len(issues)}")

    closeable = match_issues_to_enhanced(issues, matched, id_to_title, enhanced)
    print(f"Issues to close: {len(closeable)}\n")

    if not closeable:
        print("Nothing to close.")
        return

    # Step 4: Close (or dry-run)
    if do_close:
        print("Closing issues...\n")
    else:
        print("Dry-run mode (use --close to actually close):\n")

    close_issues(closeable, dry_run=not do_close)

    if not do_close:
        print(f"\nRun with --close to close these {len(closeable)} issues.")


if __name__ == "__main__":
    main()
