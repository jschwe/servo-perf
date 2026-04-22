// HTTP/2 fixture for servoperf. Usage:  node h2_server.js <port> <doc_root_abs>
const http2 = require('http2');
const fs = require('fs');
const path = require('path');

const port = parseInt(process.argv[2], 10);
const docRoot = path.resolve(process.argv[3]);
const here = __dirname;

const srv = http2.createSecureServer({
  key: fs.readFileSync(path.join(here, 'key.pem')),
  cert: fs.readFileSync(path.join(here, 'cert.pem')),
  allowHTTP1: true,
});

srv.on('stream', (stream, headers) => {
  const p = headers[':path'];
  const fp = path.join(docRoot, p === '/' ? 'index.html' : p);
  let data;
  try {
    data = fs.readFileSync(fp);
  } catch {
    stream.respond({ ':status': 404 });
    stream.end('nf');
    return;
  }
  const ext = path.extname(fp).toLowerCase();
  const ct = ext === '.html' ? 'text/html'
           : ext === '.css'  ? 'text/css'
           : ext === '.js'   ? 'application/javascript'
           : ext === '.png'  ? 'image/png'
           : 'application/octet-stream';
  console.log(`[srv ${(Date.now() / 1000).toFixed(3)}] ${headers[':method']} ${p} (${data.length}B)`);
  stream.respond({ ':status': 200, 'content-type': ct, 'content-length': data.length });
  stream.end(data);
});

srv.listen(port, '127.0.0.1', () => {
  console.log(`h2 listening on https://127.0.0.1:${port}/ doc_root=${docRoot}`);
});
