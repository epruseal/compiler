"""NUL in a cached source: the tools parse the source without NUL, warn once, and hash the raw bytes."""
import contextlib
import hashlib
import io
import os
import sys
import tempfile
import unittest
import xml.etree.ElementTree as ET
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
sys.path.insert(0, str(ROOT / 'measure'))
import full_scan  # noqa: E402
import source_text  # noqa: E402

FIXTURES = Path(__file__).resolve().parent / 'fixtures'
NUL_FIXTURE = FIXTURES / 'nul-in-cdata.xml'
NUL_FIXTURE_SHA256 = '9e22859f6e0ca79c00d77ec02b125e6db73374ed94df5fa25b2533bba5acd8e0'
WARNING = 'NUL character(s)'


def warning(count, source):
    return f'warning: removed {count} NUL character(s) from {source} before parsing'


def no_nul(value):
    if isinstance(value, str):
        return '\0' not in value
    if isinstance(value, dict):
        return all(no_nul(k) and no_nul(v) for k, v in value.items())
    if isinstance(value, (list, tuple)):
        return all(no_nul(v) for v in value)
    return True


def read_quietly(path, dataset):
    err = io.StringIO()
    with contextlib.redirect_stderr(err):
        result = source_text.read_source(path, dataset)
    return result, err.getvalue()


def scan_quietly(path):
    err = io.StringIO()
    with contextlib.redirect_stderr(err):
        row = full_scan.scan(('ordinances', os.path.dirname(path), os.path.basename(path), 1))
    return row, err.getvalue()


class ReadSourceTest(unittest.TestCase):
    def test_fixture_is_the_shared_bytes(self):
        """Control (passes before the fix too): the fixture keeps the shared bytes."""
        data = NUL_FIXTURE.read_bytes()
        self.assertEqual(hashlib.sha256(data).hexdigest(), NUL_FIXTURE_SHA256)
        self.assertEqual(data.count(b'\0'), 3)

    def test_read_source_parses_without_nul_and_hashes_the_raw_bytes(self):
        (meta, fields), err = read_quietly(NUL_FIXTURE, 'ordinances')
        self.assertEqual(meta['source_sha256'], NUL_FIXTURE_SHA256)
        self.assertEqual(meta['source_bytes'], NUL_FIXTURE.stat().st_size)
        self.assertTrue(no_nul(meta), meta)
        self.assertTrue(no_nul(fields), fields)
        self.assertIn('시험을 목적으로', fields['조문'])
        self.assertEqual(err.splitlines(), [warning(3, NUL_FIXTURE)])

    def test_read_source_failure_reports_the_parse_error_of_the_stripped_bytes(self):
        raw = b'<root>A\0<a></root>'
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / 'bad.xml'
            path.write_bytes(raw)
            err = io.StringIO()
            with contextlib.redirect_stderr(err), self.assertRaises(ET.ParseError) as caught:
                source_text.read_source(path, 'ordinances')
        with self.assertRaises(ET.ParseError) as expected:
            ET.fromstring(raw.replace(b'\0', b''))
        self.assertIn('mismatched tag', str(expected.exception))
        self.assertEqual(str(caught.exception), str(expected.exception))
        self.assertNotIn(WARNING, err.getvalue())

    def test_control_nul_free_fixture_is_unchanged(self):
        """Control (passes before the fix too)."""
        path = FIXTURES / '1081693.xml'
        data = path.read_bytes()
        self.assertNotIn(b'\0', data)
        (meta, fields), err = read_quietly(path, 'ordinances')
        self.assertEqual(meta['source_sha256'], hashlib.sha256(data).hexdigest())
        self.assertEqual(meta['source_bytes'], len(data))
        self.assertEqual(err, '')


class ScanTest(unittest.TestCase):
    def test_scan_reads_meta_without_nul_and_hashes_the_raw_bytes(self):
        raw = ('<LawService><자치법규기본정보><자치법규ID>2000159</자치법규ID>'
               '<자치법규일련번호>4800\0' '37</자치법규일련번호><자치법규종류>C0001</자치법규종류>'
               '<공포일자>19990315</공포일자><시행일자>99991231</시행일자></자치법규기본정보>'
               '<조문><조><조내용><![CDATA[제1조 목\0적]]></조내용></조></조문></LawService>').encode()
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, 'nul.xml')
            Path(path).write_bytes(raw)
            row, err = scan_quietly(path)
        self.assertEqual(row['serial'], '480037')
        self.assertEqual(row['eff_class'], 'sentinel_99991231')
        self.assertEqual(row['sha256'], hashlib.sha256(raw).hexdigest())
        self.assertEqual(row['bytes'], len(raw))
        self.assertTrue(no_nul(row), row)
        self.assertEqual(err.splitlines(), [warning(2, path)])

    def test_control_scan_head_boundary_uses_the_raw_length(self):
        """Control (passes before the fix too): NUL inside the head does not skip the full read."""
        head = '<LawService><자치법규기본정보><자치법규일련번호>1\0</자치법규일련번호>'.encode()
        pad = b' ' * (full_scan.HEAD + 100 - len(head))
        raw = head + pad + '<시행일자>20200101</시행일자></자치법규기본정보></LawService>'.encode()
        self.assertGreater(raw.index('<시행일자>'.encode()), full_scan.HEAD)
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, 'boundary.xml')
            Path(path).write_bytes(raw)
            row, _ = scan_quietly(path)
        self.assertEqual(row['eff_raw'], '20200101')

    def test_control_nul_free_file_has_no_warning(self):
        """Control (passes before the fix too)."""
        path = str(FIXTURES / '1081693.xml')
        row, err = scan_quietly(path)
        self.assertNotIn('error', row)
        self.assertEqual(err, '')


if __name__ == '__main__':
    unittest.main()
