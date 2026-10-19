import importlib.util
import json
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).parent
spec = importlib.util.spec_from_file_location("update_offsets", HERE / "update_offsets.py")
update_offsets = importlib.util.module_from_spec(spec)
spec.loader.exec_module(update_offsets)


class UpdaterTests(unittest.TestCase):
    def setUp(self):
        self.ini = """[Miscellaneous]\ncl_entitylist=0x1\nLocalPlayer=0x2\nGlobalVars=0x3\nInputSystem=0x4\nNameList=0x5\nViewRender=0x6\nViewMatrix=0x7\nClientState=0x8\nSignonState=0x9\nLevelName=0xa\nHighlightSettings=0xb\nNetworkVarTablePtr=0xc\nLocalEntityHandle=0xd\n[NetworkedStringTables]\nWeaponNames=0xe\n[Buttons]\nin_attack=0xf\nin_jump=0x10\nin_duck=0x11\nin_forward=0x12\nin_backward=0x13\n"""
        self.payload = {"offsets": {"structs": {"misc": {}}}, "offsets_flat": {
            "misc.cl_entitylist": "0x100",
            "misc.LocalPlayer": "0x200",
        }}

    def test_merge_updates_native_keys_and_preserves_unpublished_fields(self):
        with tempfile.TemporaryDirectory() as d:
            path = Path(d) / "offsets.ini"
            path.write_text(self.ini)
            original, changed, updates, unmatched = update_offsets.merge(path, self.payload)
            self.assertEqual(2, len(updates))
            self.assertIn("cl_entitylist=0x100", changed)
            self.assertIn("LocalPlayer=0x200", changed)
            self.assertIn("GlobalVars=0x3", changed)
            self.assertEqual([], unmatched)
            self.assertNotEqual(original, changed)

    def test_candidate_validation_rejects_missing_required_key(self):
        broken = self.ini.replace("ViewMatrix=0x7\n", "")
        with self.assertRaises(ValueError):
            update_offsets.validate_candidate(broken)

    def test_candidate_validation_rejects_non_hex_required_value(self):
        broken = self.ini.replace("ViewMatrix=0x7", "ViewMatrix=not-an-offset")
        with self.assertRaises(ValueError):
            update_offsets.validate_candidate(broken)

    def test_malformed_api_value_is_not_applied(self):
        payload = {"offsets": {"structs": {"misc": {}}}, "offsets_flat": {
            "misc.LocalPlayer": "not-hex",
            "misc.GlobalVars": "0x999",
        }}
        with tempfile.TemporaryDirectory() as d:
            path = Path(d) / "offsets.ini"
            path.write_text(self.ini)
            _, changed, updates, _ = update_offsets.merge(path, payload)
            self.assertEqual(1, len(updates))
            self.assertIn("GlobalVars=0x999", changed)
            self.assertIn("LocalPlayer=0x2", changed)


if __name__ == "__main__":
    unittest.main()
