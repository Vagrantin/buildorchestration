import gzip
import importlib.util
import io
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest import mock
import xmlrpc.client

spec = importlib.util.spec_from_file_location('normalize_xva', Path(__file__).resolve().parents[1]/'src/normalize_xva.py')
module = importlib.util.module_from_spec(spec); spec.loader.exec_module(module)

def metadata(label='qa-xoa-build-56', classes=('VM',)):
    value = {'objects': [{'class': cls, 'id': str(i), 'snapshot': {'name_label': label, 'uuid':'fixture', 'other_config':{'fixture':'untouched'}}} for i, cls in enumerate(classes)]}
    # XAPI's XML-RPC value root (rather than a method call).
    return xmlrpc.client.dumps((value,)).split('<param>')[1].split('</param>')[0].strip().encode()

def archive(data, first='ova.xml'):
    out = io.BytesIO()
    with tarfile.open(fileobj=out, mode='w') as target:
        for name, body in [(first,data), ('Ref:1/00000000', b'fixture disk\0'*4096), ('Ref:1/00000000.checksum',b'unchanged checksum')]:
            info = tarfile.TarInfo(name); info.size=len(body); target.addfile(info,io.BytesIO(body))
    return out.getvalue()

class Normalization(unittest.TestCase):
    def test_plain_and_gzip_keep_disk_remainder_and_identity(self):
        for compressed in (False, True):
            with self.subTest(compressed=compressed), tempfile.TemporaryDirectory() as directory:
                original = archive(metadata()); path=Path(directory)/'image.xva'
                path.write_bytes(gzip.compress(original) if compressed else original)
                self.assertTrue(module.normalize(path))
                result=gzip.decompress(path.read_bytes()) if compressed else path.read_bytes()
                old_size=tarfile.TarInfo.frombuf(original[:512],'utf8','strict').size
                new_size=tarfile.TarInfo.frombuf(result[:512],'utf8','strict').size
                self.assertEqual(original[512+((old_size+511)//512)*512:],result[512+((new_size+511)//512)*512:])
                before=xmlrpc.client.loads(b"<params><param>"+metadata()+b"</param></params>")[0][0]
                with tarfile.open(fileobj=io.BytesIO(result)) as source:
                    after=xmlrpc.client.loads(b'<params><param>'+module.ET.tostring(module.ET.fromstring(source.extractfile('ova.xml').read()))+b'</param></params>')[0][0]
                before['objects'][0]['snapshot']['name_label']='XOA-hl'
                self.assertEqual(before,after)
                self.assertFalse(module.normalize(path))
                self.assertEqual(list(Path(directory).iterdir()),[path])

    def test_bad_inputs_leave_original_bytes(self):
        cases=[b'bad', archive(metadata(),first='disk'),archive(metadata(classes=('VDI',))),archive(metadata(classes=('VM','VM'))),archive(b'<bad>'),archive(b'<!DOCTYPE value><value/>')]
        for data in cases:
            with self.subTest(size=len(data)), tempfile.TemporaryDirectory() as directory:
                path=Path(directory)/'image.xva';path.write_bytes(data)
                with self.assertRaises((ValueError,tarfile.HeaderError,module.ET.ParseError)):
                    module.normalize(path)
                self.assertEqual(path.read_bytes(),data)
                self.assertEqual(list(Path(directory).iterdir()),[path])

    def test_untyped_xmlrpc_strings(self):
        data=metadata().replace(b'<string>',b'').replace(b'</string>',b'')
        self.assertIn(b'XOA-hl',module.relabel(data))

    def test_corrupt_gzip_preserves_original(self):
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory)/'image.xva'
            data=bytearray(gzip.compress(archive(metadata())));data[-8] ^= 1; data=bytes(data)
            path.write_bytes(data)
            with self.assertRaises(gzip.BadGzipFile): module.normalize(path)
            self.assertEqual(path.read_bytes(),data)
            self.assertEqual(list(Path(directory).iterdir()),[path])

    def test_failed_replace_preserves_original(self):
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory)/'image.xva';data=archive(metadata());path.write_bytes(data)
            with mock.patch.object(module.os,'replace',side_effect=OSError('fixture')):
                with self.assertRaises(OSError): module.normalize(path)
            self.assertEqual(path.read_bytes(),data)
            self.assertEqual(list(Path(directory).iterdir()),[path])

if __name__ == '__main__': unittest.main()
