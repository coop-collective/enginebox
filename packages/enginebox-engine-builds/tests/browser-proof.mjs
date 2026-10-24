// Uses the repository's pinned Playwright install; no app or running fleet needed.
import { chromium } from '../../dashboard/node_modules/@playwright/test/index.mjs';
import { readFile, stat } from 'node:fs/promises';
import http from 'node:http';
import path from 'node:path';
import assert from 'node:assert/strict';
const receipt=JSON.parse(await readFile(process.argv[2],'utf8'));
const root=path.resolve(receipt.filesRoot);
const types={'.html':'text/html','.js':'text/javascript','.wasm':'application/wasm','.pck':'application/octet-stream','.png':'image/png'};
const server=http.createServer(async(req,res)=>{
  try{
    const name=decodeURIComponent(new URL(req.url,'http://localhost').pathname).replace(/^\//,'')||'index.html';
    const file=path.resolve(root,name);assert(file.startsWith(root+path.sep));assert((await stat(file)).isFile());
    res.setHeader('Content-Type',types[path.extname(file)]||'application/octet-stream');
    res.setHeader('Cross-Origin-Opener-Policy','same-origin');res.setHeader('Cross-Origin-Embedder-Policy','require-corp');
    res.end(await readFile(file));
  }catch{res.writeHead(404).end();}
});
await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
let browser;
try{
 browser=await chromium.launch({headless:true,args:['--use-gl=angle','--use-angle=swiftshader','--enable-unsafe-swiftshader']});
 const page=await browser.newPage({viewport:{width:640,height:360}});
 const errors=[];page.on('pageerror',error=>errors.push(String(error)));
 await page.goto(`http://127.0.0.1:${server.address().port}/`);
 await page.waitForFunction(()=>window.engineboxReady===true,{},{timeout:60000});
 await page.mouse.click(320,175);
 await page.waitForFunction(()=>window.engineboxPlayed===1);
 assert.deepEqual(errors,[]);
 await page.screenshot({path:path.join(path.dirname(process.argv[2]),'godot-web-playable.png')});
 console.log(JSON.stringify({artifactId:receipt.artifactId,loaded:true,played:1}));
}finally{await browser?.close();await new Promise(resolve=>server.close(resolve));}
