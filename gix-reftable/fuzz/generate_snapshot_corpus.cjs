'use strict';

const { execFileSync } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');

const root = path.resolve(__dirname, '../..');
const fixtures = path.join(root, 'gix-reftable/tests/fixtures/generated-archives');
const corpus = path.join(__dirname, 'corpus/snapshot');
const prefix = 'reftable/repo/.git/reftable/';
fs.mkdirSync(corpus, { recursive: true });

for (const [hash, archive, kind] of [
  ['sha1', 'make_ref_store.tar', 0],
  ['sha256', 'make_ref_store_sha256.tar', 1],
]) {
  const archivePath = path.join(fixtures, archive);
  const names = execFileSync('tar', ['-xOf', archivePath, `${prefix}tables.list`], { encoding: 'utf8' })
    .trimEnd()
    .split('\n');
  if (names.length !== 3) throw new Error(`${archive} must contain three ordered tables`);

  const parts = [Buffer.from([kind, names.length])];
  for (const name of names) {
    if (!/^0x[0-9a-f]+-0x[0-9a-f]+-[0-9a-f]+\.ref$/.test(name)) {
      throw new Error(`Unexpected table name ${name}`);
    }
    const table = execFileSync('tar', ['-xOf', archivePath, `${prefix}${name}`]);
    if (table.length > 0xffff) throw new Error(`${name} does not fit a 16-bit length`);
    const length = Buffer.alloc(2);
    length.writeUInt16BE(table.length);
    parts.push(length, table);
  }
  parts.push(Buffer.from('refs/heads/topic'));
  const output = path.join(corpus, `git-${hash}`);
  const bytes = Buffer.concat(parts);
  if (fs.existsSync(output)) {
    if (!fs.readFileSync(output).equals(bytes)) throw new Error(`${output} differs from the archived fixtures`);
  } else {
    fs.writeFileSync(output, bytes, { flag: 'wx' });
  }
  console.log(`${output}: ${bytes.length} bytes from ${names.length} Git tables`);
}
