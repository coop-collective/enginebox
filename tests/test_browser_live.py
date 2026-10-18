"""Opt-in real Chromium/MCP check. Run as the non-root guest user, no inference."""
import asyncio
import functools
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
import os
from pathlib import Path
import re
import sys
import tempfile
import threading
import unittest


@unittest.skipUnless(os.environ.get('ENGINEBOX_BROWSER_LIVE') == '1', 'requires an explicitly prepared disposable guest')
class LiveBrowserTests(unittest.TestCase):
    def test_native_mcp_navigation_input_screenshot_and_persistence(self):
        from mcp import Client, StdioServerParameters

        async def run(directory, url):
            root = Path(__file__).resolve().parents[1]
            helper = root / 'runtime/browser_tools.py'
            if not helper.exists(): helper = Path('/opt/enginebox-runtime/browser_tools.py')
            params = StdioServerParameters(command=sys.executable,
                args=[str(helper), '--client', 'smoke'],
                env={**os.environ, 'ENGINEBOX_BROWSER_HOME': str(directory / 'browser')}, cwd=str(directory))
            async with Client(params) as client:
                tools = {tool.name: tool.model_dump(by_alias=True) for tool in (await client.list_tools()).tools}
                names = set(tools)
                for tool in ('browser_navigate', 'browser_snapshot', 'browser_type', 'browser_click', 'browser_take_screenshot'):
                    self.assertIn(tool, names)
                result = await client.call_tool('browser_navigate', {'url': url})
                self.assertFalse(result.is_error, str(result))
                result = await client.call_tool('browser_snapshot', {})
                self.assertFalse(result.is_error, str(result))
                snapshot = '\n'.join(c.text for c in result.content if c.type == 'text')
                ref = re.search(r'textbox "Name"[^\n]*?\[ref=([^\]]+)\]', snapshot)
                self.assertIsNotNone(ref, snapshot)
                # Upstream renamed ref to target; inspect the installed package's
                # advertised schema so this also validates explicit upgrades.
                target_key = 'target' if 'target' in tools['browser_type']['inputSchema']['properties'] else 'ref'
                result = await client.call_tool('browser_type', {target_key: ref.group(1), 'element': 'Name input', 'text': 'Engine Box'})
                self.assertFalse(result.is_error, str(result))
                result = await client.call_tool('browser_snapshot', {})
                self.assertFalse(result.is_error, str(result))
                snapshot = '\n'.join(c.text for c in result.content if c.type == 'text')
                ref = re.search(r'button "Save"[^\n]*?\[ref=([^\]]+)\]', snapshot)
                self.assertIsNotNone(ref, snapshot)
                target_key = 'target' if 'target' in tools['browser_click']['inputSchema']['properties'] else 'ref'
                result = await client.call_tool('browser_click', {target_key: ref.group(1), 'element': 'Save button'})
                self.assertFalse(result.is_error, str(result))
                result = await client.call_tool('browser_snapshot', {})
                self.assertIn('Saved Engine Box', '\n'.join(c.text for c in result.content if c.type == 'text'))
                screenshot_args = {'type': 'png', 'filename': 'smoke.png'}
                if 'scale' in tools['browser_take_screenshot']['inputSchema']['properties']: screenshot_args['scale'] = 'css'
                result = await client.call_tool('browser_take_screenshot', screenshot_args)
                self.assertFalse(result.is_error, str(result))
                self.assertTrue((directory / 'smoke.png').read_bytes().startswith(b'\x89PNG\r\n\x1a\n'))
                await client.call_tool('browser_close', {})
            # A fresh official MCP process reuses this profile's browser storage.
            async with Client(params) as client:
                result = await client.call_tool('browser_navigate', {'url': url})
                self.assertFalse(result.is_error, str(result))
                result = await client.call_tool('browser_snapshot', {})
                self.assertFalse(result.is_error, str(result))
                self.assertIn('Saved Engine Box', '\n'.join(c.text for c in result.content if c.type == 'text'))
                await client.call_tool('browser_close', {})

        with tempfile.TemporaryDirectory(prefix='enginebox-browser-smoke-') as temporary:
            directory = Path(temporary)
            (directory / 'index.html').write_text('''<!doctype html><title>Engine Box browser smoke</title>
<label>Name <input id="name"></label><button onclick="localStorage.setItem('saved', document.querySelector('input').value); show()">Save</button><p id="result"></p>
<script>function show(){document.querySelector('#result').textContent='Saved '+(localStorage.getItem('saved') || 'nothing')}show()</script>''')
            class QuietHandler(SimpleHTTPRequestHandler):
                def log_message(self, *args): pass
            handler = functools.partial(QuietHandler, directory=temporary)
            server = ThreadingHTTPServer(('127.0.0.1', 0), handler)
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                asyncio.run(run(directory, f'http://127.0.0.1:{server.server_port}/'))
            finally:
                server.shutdown()
                server.server_close()
                thread.join(timeout=5)


if __name__ == '__main__':
    unittest.main()
