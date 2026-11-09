"""Set only the exported VM label; retain the build VM's diagnostic name (#291).

XAPI exports ova.xml as the first tar member; disk chunks/checksums follow.
Copy that remainder byte-for-byte in bounded chunks, preserving outer gzip.
"""
import contextlib
import gzip
import os
from pathlib import Path
import shutil
import sys
import tarfile
import tempfile
import xml.etree.ElementTree as ET

MAX_METADATA = 16 * 1024 * 1024

def member(struct, name):
    values = [m.find('value') for m in struct.findall('member') if m.findtext('name') == name]
    if len(values) != 1 or values[0] is None:
        raise ValueError(f'expected one {name} member')
    return values[0]

def text(value):
    string = value.find('string')
    return string.text if string is not None else value.text

def relabel(data):
    if b'<!DOCTYPE' in data.upper() or b'<!ENTITY' in data.upper():
        raise ValueError('unexpected XML declarations')
    root = ET.fromstring(data)
    objects = []
    for struct in root.iter('struct'):
        classes = [m.find('value') for m in struct.findall('member') if m.findtext('name') == 'class']
        if len(classes) == 1 and classes[0] is not None and text(classes[0]) == 'VM':
            objects.append(struct)
    if len(objects) != 1:
        raise ValueError('expected exactly one exported VM')
    snapshot = member(objects[0], 'snapshot').find('struct')
    if snapshot is None:
        raise ValueError('VM snapshot is not a struct')
    value = member(snapshot, 'name_label')
    if text(value) == 'XOA-hl':
        return data
    string = value.find('string')
    if string is not None:
        string.text = 'XOA-hl'
    elif len(value) == 0:
        value.text = 'XOA-hl'
    else:
        raise ValueError('name_label is not a string')
    return ET.tostring(root, encoding='utf-8', xml_declaration=True)

def read_exact(stream, size):
    data = stream.read(size)
    if len(data) != size:
        raise ValueError('truncated XVA metadata')
    return data

def normalize(path):
    path = Path(path)
    temporary = None
    try:
        with contextlib.ExitStack() as stack:
            raw = stack.enter_context(path.open('rb'))
            magic = raw.read(2); raw.seek(0)
            compressed = magic == b'\x1f\x8b'
            source = stack.enter_context(gzip.GzipFile(fileobj=raw)) if compressed else raw
            header = read_exact(source, 512)
            info = tarfile.TarInfo.frombuf(header, 'utf-8', 'strict')
            if info.name != 'ova.xml' or not info.isfile() or not 0 < info.size <= MAX_METADATA:
                raise ValueError('first XVA member must be a bounded regular ova.xml')
            data = read_exact(source, info.size)
            read_exact(source, (-info.size) % 512)
            updated = relabel(data)
            if updated == data:
                return False
            fd, temporary = tempfile.mkstemp(prefix=path.name+'.label-', dir=path.parent)
            target_raw = stack.enter_context(os.fdopen(fd, 'wb'))
            target = stack.enter_context(gzip.GzipFile(fileobj=target_raw, mode='wb', filename='', mtime=0, compresslevel=1)) if compressed else target_raw
            info.size = len(updated)
            target.write(info.tobuf(format=tarfile.USTAR_FORMAT))
            target.write(updated)
            target.write(b'\0' * ((-len(updated)) % 512))
            shutil.copyfileobj(source, target, length=1024 * 1024)
            if compressed:
                target.close()
            target_raw.flush(); os.fsync(target_raw.fileno())
            os.fchmod(target_raw.fileno(), path.stat().st_mode & 0o777)
        os.replace(temporary, path)
        temporary = None
        return True
    finally:
        if temporary is not None:
            os.unlink(temporary)

if __name__ == '__main__':
    changed = normalize(sys.argv[1])
    print('XVA VM label: XOA-hl' + (' (normalized)' if changed else ' (already correct)'))
