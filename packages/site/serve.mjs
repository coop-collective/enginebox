import { createServer } from 'node:http';
import { readFile, stat } from 'node:fs/promises';
import { extname, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';
const root = resolve(fileURLToPath(new URL('./dist/', import.meta.url)));
const port = Number(process.env.PORT || 4178);
const types = {'.html':'text/html; charset=utf-8','.css':'text/css; charset=utf-8','.js':'text/javascript; charset=utf-8','.mjs':'text/javascript; charset=utf-8','.json':'application/json; charset=utf-8','.svg':'image/svg+xml','.txt':'text/plain; charset=utf-8'};
const server = createServer(async (req,res) => {
  if (!['GET','HEAD'].includes(req.method)) { res.writeHead(405); res.end(); return; }
  try {
    const url = new URL(req.url,'http://localhost');
    const path = decodeURIComponent(url.pathname);
    let file = resolve(root,'.'+path);
    if ((file !== root && !file.startsWith(root + sep)) || file.includes('\0')) { res.writeHead(400); res.end(); return; }
    const info = await stat(file);
    if (info.isDirectory()) {
      if (!url.pathname.endsWith('/')) {res.writeHead(308,{Location:url.pathname+'/'+url.search});res.end();return;}
      file = resolve(file,'index.html');
    }
    const body = await readFile(file);
    res.writeHead(200,{'Content-Type':types[extname(file)] || 'application/octet-stream','X-Content-Type-Options':'nosniff'});
    res.end(req.method === 'HEAD' ? undefined : body);
  } catch {
    const body = await readFile(resolve(root,'404.html')).catch(()=>'Not found');
    res.writeHead(404,{'Content-Type':'text/html; charset=utf-8'});res.end(req.method === 'HEAD' ? undefined : body);
  }
});
server.listen(port,'127.0.0.1',()=>console.log('Engine Box site: http://127.0.0.1:'+port));
