import assert from 'node:assert/strict';
import { QueryEngine } from '@comunica/query-sparql';

const endpoint = process.argv[2];
const remoteEndpoint = process.argv[3];
assert(endpoint, 'usage: node test.mjs http://host/dataset/v/version/tpf');
assert(remoteEndpoint, 'a second KGF TPF endpoint is required for federation');

const engine = new QueryEngine();
const context = {
  sources: [{ type: 'brtpf', value: endpoint }],
};

const negotiated = await fetch(endpoint, {
  headers: {
    accept: 'application/n-quads, application/trig;q=0.95, application/ld+json;q=0.9, application/n-triples;q=0.8, text/turtle;q=0.6',
  },
});
assert.equal(negotiated.status, 200);
assert.equal(
  negotiated.headers.get('content-type'),
  'application/n-quads; charset=utf-8',
  'Comunica\'s RDF preference list must negotiate N-Quads',
);
await negotiated.arrayBuffer();

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

assert.deepEqual(
  await rows(`
    SELECT ?person WHERE {
      ?person <http://example.org/born> "1998-04-20"^^<http://www.w3.org/2001/XMLSchema#date>
    }
  `, ['person']),
  [{ person: 'http://example.org/alice' }],
  'a TPF typed literal keeps its bare datatype IRI through brTPF ingress',
);

const blankNodeJoin = await rows(`
  SELECT ?node ?kind WHERE {
    ?node <http://example.org/type> <http://example.org/Thing> .
    ?node <http://example.org/type> ?kind .
  }
`, ['node', 'kind']);
assert.equal(
  blankNodeJoin.length,
  1,
  'the same stored blank node must join across fragment documents and brTPF bindings',
);
assert.equal(blankNodeJoin[0].kind, 'http://example.org/Thing');
assert.match(
  blankNodeJoin[0].node,
  /^urn:fdc:frink-okn\.github\.io:20260818:kgf:bnode:v1:sha256:[0-9a-f]{64}:(?:sh|s|o)-[1-9][0-9]*$/,
  'fragment data blank nodes must have stable content-scoped identities',
);

assert.deepEqual(
  await rows(`
    SELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }
  `, ['n']),
  [{ n: '9' }],
  'the default graph must contain exactly the nine fixture triples and no controls',
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

console.log('Comunica 5.3.0 TPF grammar, paging, graph split, bind joins, and federation passed');
