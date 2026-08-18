import assert from 'node:assert/strict';
import { QueryEngine } from '@comunica/query-sparql';

const endpoint = process.argv[2];
const remoteEndpoint = process.argv[3];
assert(endpoint, 'usage: node test.mjs http://host/dataset/v/version/fragment');
assert(remoteEndpoint, 'a second KGF fragment endpoint is required for federation');

const engine = new QueryEngine();
const context = {
  sources: [{ type: 'brtpf', value: endpoint }],
};

async function rows(query, variables) {
  const stream = await engine.queryBindings(query, context);
  const bindings = await stream.toArray();
  return bindings
    .map(binding => Object.fromEntries(variables.map(variable => [
      variable,
      binding.get(variable)?.value,
    ])))
    .sort((left, right) => JSON.stringify(left).localeCompare(JSON.stringify(right)));
}

assert.deepEqual(
  await rows(`
    SELECT ?person WHERE {
      ?person <http://example.org/knows> <http://example.org/bob>
    }
  `, ['person']),
  [{ person: 'http://example.org/alice' }],
  'stock Comunica must discover and page the three-map TPF form',
);

assert.deepEqual(
  await rows(`
    SELECT ?person ?knownName WHERE {
      ?person <http://example.org/knows> ?known .
      ?known <http://example.org/name> ?knownName .
    }
  `, ['person', 'knownName']),
  [
    { person: 'http://example.org/alice', knownName: 'Bob' },
    { person: 'http://example.org/bob', knownName: 'Alice' },
  ],
  'stock Comunica must complete its bind join through brTPF values= requests',
);

const federatedStream = await engine.queryBindings(`
  SELECT ?person ?remoteName WHERE {
    ?person <http://example.org/knows> ?known .
    ?known <http://example.org/remoteName> ?remoteName .
  }
`, {
  sources: [
    { type: 'brtpf', value: endpoint },
    { type: 'brtpf', value: remoteEndpoint },
  ],
});
const federated = (await federatedStream.toArray()).map(binding => ({
  person: binding.get('person')?.value,
  remoteName: binding.get('remoteName')?.value,
}));
assert.deepEqual(
  federated,
  [{ person: 'http://example.org/alice', remoteName: 'Bobby' }],
  'stock Comunica must join bindings across two KGF brTPF endpoints',
);

console.log('Comunica 5.3.0 TPF paging, brTPF bind join, and federation passed');
