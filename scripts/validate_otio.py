"""Check Depthcut's OTIO export against the reference opentimelineio library,
and write a library-authored file for the import test fixture.

    python -m venv .otio && .otio/Scripts/pip install opentimelineio
    RCP_OTIO_DUMP=<dir> cargo test -p nle-core otio::tests::round_trip_keeps_structure
    .otio/Scripts/python scripts/validate_otio.py <dir>/rcp_export.otio [fixture-out.otio]

Exits non-zero (with the reason) if the library rejects or misreads the export.
"""
import sys

import opentimelineio as otio

RT = otio.opentime.RationalTime
TR = otio.opentime.TimeRange


def check(cond, msg):
    if not cond:
        sys.exit(f"FAIL: {msg}")
    print(f"ok   {msg}")


def validate(path):
    tl = otio.adapters.read_from_file(path)
    check(isinstance(tl, otio.schema.Timeline), f"loads as a Timeline ({otio.__version__})")
    check(tl.name == "Doc Cut", "timeline name")
    check(otio.opentime.to_timecode(tl.global_start_time) == "01:00:00:00", f"global start 01:00:00:00 ({tl.global_start_time})")
    names = [t.name for t in tl.tracks]
    check(names == ["V1", "V2", "A1", "A2"], f"tracks {names}")
    kinds = [t.kind for t in tl.tracks]
    check(kinds == ["Video", "Video", "Audio", "Audio"], f"kinds {kinds}")

    v1 = tl.tracks[0]
    clips = list(v1.find_clips())
    check(len(clips) == 2, "V1 has two clips")
    a = clips[0]
    check(a.source_range == TR(RT(72, 24), RT(120, 24)), f"clip 1 range {a.source_range}")
    check(a.media_reference.target_url == "file:///D:/Footage/Day%201/A%20001.mov", f"clip 1 url {a.media_reference.target_url}")
    trans = [c for c in v1 if isinstance(c, otio.schema.Transition)]
    check(len(trans) == 1 and trans[0].transition_type == otio.schema.TransitionTypes.SMPTE_Dissolve, "one SMPTE dissolve")
    t = trans[0]
    check((t.in_offset + t.out_offset).to_frames() == 12, f"dissolve 12 frames ({t.in_offset}+{t.out_offset})")
    b = clips[1]
    warps = [e for e in b.effects if isinstance(e, otio.schema.LinearTimeWarp)]
    check(len(warps) == 1 and warps[0].time_scalar == 2.0, "clip 2 LinearTimeWarp x2")
    check(v1.duration() == RT(216, 24), f"V1 duration {v1.duration()} (5 s + 4 s)")

    # the title is a placeholder clip of the right length
    v2 = tl.tracks[1]
    titles = list(v2.find_clips())
    check(len(titles) == 1 and isinstance(titles[0].media_reference, otio.schema.MissingReference), "title = MissingReference clip")
    check(v2.range_of_child(titles[0]).start_time == RT(24, 24), "title starts at 1 s")

    # music on A2 starts at 2 s after a gap
    a2 = tl.tracks[3]
    m = list(a2.find_clips())
    check(len(m) == 1 and m[0].name == "score.wav", "music on A2")
    check(a2.range_of_child(m[0]).start_time == RT(48, 24), "music starts at 2 s")
    check(tl.duration() == RT(216, 24), f"timeline duration {tl.duration()}")

    # survives a library write / read round trip
    again = otio.adapters.read_from_string(otio.adapters.write_to_string(tl))
    check(again.is_equivalent_to(tl), "library round trip is equivalent")


def write_fixture(path):
    """A 29.97 timeline written by the library itself, using what Resolve /
    Premiere exports contain: Clip.2 with several media references, gaps,
    transitions, a time warp, markers and a nested stack."""
    r = 30000 / 1001
    tl = otio.schema.Timeline(name="Library Cut", global_start_time=RT(107892, r))
    v1 = otio.schema.Track(name="V1", kind=otio.schema.TrackKind.Video)
    ref = otio.schema.ExternalReference(target_url="file:///C:/Media/Interview%20A.mov", available_range=TR(RT(0, r), RT(3000, r)))
    c1 = otio.schema.Clip(name="Interview A", source_range=TR(RT(300, r), RT(150, r)))
    c1.set_media_references({"high": ref, "proxy": otio.schema.ExternalReference(target_url="file:///C:/Proxy/Interview%20A.mp4")}, "high")
    c1.markers.append(otio.schema.Marker(name="note", marked_range=TR(RT(310, r), RT(1, r))))
    v1.append(c1)
    v1.append(otio.schema.Transition(transition_type=otio.schema.TransitionTypes.SMPTE_Dissolve, in_offset=RT(10, r), out_offset=RT(5, r)))
    c2 = otio.schema.Clip(name="B-roll", media_reference=otio.schema.ExternalReference(target_url="file:///C:/Media/broll.mp4"), source_range=TR(RT(0, r), RT(90, r)))
    c2.effects.append(otio.schema.LinearTimeWarp(time_scalar=0.5))
    v1.append(c2)
    v1.append(otio.schema.Gap(source_range=TR(RT(0, r), RT(30, r))))
    nested = otio.schema.Stack(name="compound", source_range=TR(RT(0, r), RT(60, r)))
    v1.append(nested)
    v1.append(otio.schema.Clip(name="Missing", media_reference=otio.schema.MissingReference(), source_range=TR(RT(0, r), RT(45, r))))
    c3 = otio.schema.Clip(name="Interview A", media_reference=ref.clone(), source_range=TR(RT(900, r), RT(120, r)))
    v1.append(c3)
    a1 = otio.schema.Track(name="A1", kind=otio.schema.TrackKind.Audio)
    a1.append(otio.schema.Gap(source_range=TR(RT(0, r), RT(60, r))))
    a1.append(otio.schema.Clip(name="music.wav", media_reference=otio.schema.ExternalReference(target_url="file:///C:/Music/music.wav"), source_range=TR(RT(0, r), RT(240, r))))
    tl.tracks.append(v1)
    tl.tracks.append(a1)
    otio.adapters.write_to_file(tl, path)
    print(f"wrote {path}: V1 {v1.duration().to_frames()} frames, A1 {a1.duration().to_frames()} frames")


if __name__ == "__main__":
    validate(sys.argv[1])
    if len(sys.argv) > 2:
        write_fixture(sys.argv[2])
