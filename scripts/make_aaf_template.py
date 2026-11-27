"""Dev-time: write (1) the empty AAF template that `crates/nle-core/src/aaf.rs`
fills in and (2) a sample with the object layout the Rust writer mirrors.

    python -m venv .aaf && .aaf/Scripts/pip install pyaaf2
    .aaf/Scripts/python scripts/make_aaf_template.py template crates/nle-core/assets/aaf_template.aaf
    .aaf/Scripts/python scripts/make_aaf_template.py sample sample.aaf
"""
import sys, aaf2

def file_mob(f, name, path, rate, length, kinds):
    src = f.create.SourceMob(name)
    src.descriptor = f.create.ImportDescriptor()
    loc = f.create.NetworkLocator()
    loc['URLString'].value = path
    src.descriptor['Locator'].append(loc)
    for i, k in enumerate(kinds):
        slot = src.create_empty_slot(rate, media_kind=k, slot_id=i + 1)
        slot.segment.length = length
    f.content.mobs.append(src)
    mm = f.create.MasterMob(name)
    f.content.mobs.append(mm)
    for i, k in enumerate(kinds):
        slot = mm.create_timeline_slot(rate, slot_id=i + 1)
        slot.segment = src.create_source_clip(slot_id=i + 1, media_kind=k)
    return mm

mode = sys.argv[1]
out = sys.argv[2]
with aaf2.open(out, 'w') as f:
    rate = 30
    comp = f.create.CompositionMob("Timeline")
    f.content.mobs.append(comp)
    for k, sid in (('picture', 1), ('sound', 2)):
        seq = f.create.Sequence(media_kind=k)
        slot = comp.create_timeline_slot(rate, slot_id=sid)
        slot.segment = seq
        seq.components.append(f.create.Filler(k, 30))
    if mode == 'template':
        # keep the dictionary entries and weak-reference table, drop the mob
        f.content.mobs.pop(comp.mob_id)
        d = f.cfb.find('/Header-2/Content-3b03/Mobs-1901{0}')
        if d is not None:
            f.cfb.rmtree(d)
        ident = f.header['IdentificationList'][0]
        ident['ProductName'].value = 'Depthcut'
        ident['CompanyName'].value = 'Depthcut'
    else:
        a = file_mob(f, 'clip a', 'file:///C:/media/a.mov', rate, 300, ['picture', 'sound'])
        seq = comp.slot_at(1).segment
        seq.components.append(a.create_source_clip(slot_id=1, start=15, length=60))
        aseq = comp.slot_at(2).segment
        aseq.components.append(a.create_source_clip(slot_id=2, start=15, length=60))
print('ok', mode)
