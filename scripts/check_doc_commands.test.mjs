import test from 'node:test';
import assert from 'node:assert/strict';
import { validateMarkdown } from './check_doc_commands.mjs';

const interfaces = {
    amm: [{ function_v0: { name: 'initialize', inputs: [
        { name: 'admin', type: 'address' }, { name: 'pool', type: 'address' },
        { name: 'governance_hash', type: { option: { value_type: 'bytes' } } },
    ] } }, { function_v0: { name: 'pause', inputs: [] } }],
    factory: [{ function_v0: { name: 'create_pool', inputs: [
        { name: 'caller', type: 'address' }, { name: 'token_a', type: 'address' },
        { name: 'token_b', type: 'address' }, { name: 'fee_tier', type: 'i128' },
    ] } }, { function_v0: { name: 'initialize', inputs: [{ name: 'admin', type: 'address' }] } }],
    governance: [{ function_v0: { name: 'vote', inputs: [
        { name: 'voter', type: 'address' }, { name: 'proposal_id', type: 'u32' },
        { name: 'choice', type: { udt: { name: 'Vote' } } },
    ] } }],
};

function check(body) {
    return validateMarkdown(body, 'fixture.md', interfaces);
}

test('parses shell continuations and a prompt command', () => {
    assert.deepEqual(check('## Factory\n```sh\n$ stellar contract invoke \\\n  --id <ID> \\\n  -- create_pool \\\n  --caller <CALLER> --token_a <A> --token_b <B> --fee_tier 30\n```'), []);
});

test('reports an ambiguous function when several contracts expose it', () => {
    const errors = check('```sh\nstellar contract invoke --id <ID> -- initialize --admin <ADMIN>\n```');
    assert.match(errors.join('\n'), /ambiguous contract \(amm, factory\)/);
});

test('treats Option ABI parameters as optional', () => {
    assert.deepEqual(check('## AMM\n```sh\nstellar contract invoke --id <ID> -- initialize --admin <ADMIN> --pool <POOL>\n```'), []);
});

test('reports global flags placed after the delimiter', () => {
    const errors = check('## AMM\n```sh\nstellar contract invoke --id <ID> -- pause --source <KEY>\n```');
    assert.match(errors.join('\n'), /global flag --source is after --/);
});

test('uses an explicit contract annotation', () => {
    assert.deepEqual(check('<!-- contract: factory -->\n```sh\nstellar contract invoke --id <ID> -- initialize --admin <ADMIN>\n```'), []);
});

test('reports unknown functions', () => {
    const errors = check('## AMM\n```sh\nstellar contract invoke --id <ID> -- not_a_function\n```');
    assert.match(errors.join('\n'), /not_a_function: unknown function/);
});

test('reports unknown flags with ABI parameter names', () => {
    const errors = check('## Factory\n```sh\nstellar contract invoke --id <ID> -- create_pool --caller <CALLER> --token_a <A> --token_b <B> --fee_bps 30\n```');
    assert.match(errors.join('\n'), /unknown flag --fee_bps; ABI parameters: --caller, --token_a, --token_b, --fee_tier/);
});

test('reports missing required parameters by ABI name', () => {
    const errors = check('## Factory\n```sh\nstellar contract invoke --id <ID> -- create_pool --caller <CALLER> --token_a <A>\n```');
    assert.match(errors.join('\n'), /missing required flag --token_b; ABI parameter: token_b/);
    assert.match(errors.join('\n'), /missing required flag --fee_tier; ABI parameter: fee_tier/);
});

test('reports contract arguments placed before the delimiter', () => {
    const errors = check('## AMM\n```sh\nstellar contract invoke --id <ID> --admin <ADMIN> -- pause\n```');
    assert.match(errors.join('\n'), /contract flag --admin is before --/);
});

test('catches a renamed required argument', () => {
    const errors = check('## Factory\n```sh\nstellar contract invoke --id <ID> -- create_pool --caller <CALLER> --token_a <A> --token_b <B> --fee_bps 30\n```');
    assert.match(errors.join('\n'), /unknown flag --fee_bps/);
    assert.match(errors.join('\n'), /missing required flag --fee_tier/);
});

test('checks contractclient trait argument names against the ABI', () => {
    const errors = check('```rust\n#[contractclient(name = "FactoryClient")]\npub trait Factory {\n    fn create_pool(env: Env, caller: Address, token_a: Address, token_b: Address, fee_bps: i128);\n}\n```');
    assert.match(errors.join('\n'), /Rust trait create_pool: arguments \(env: Env, caller: Address, token_a: Address, token_b: Address, fee_bps: i128\) do not match ABI parameters/);
});

test('checks the flash-loan callback against the AMM trait', () => {
    const errors = check('```rust\npub trait FlashLoanReceiver {\n    fn on_flash_loan(env: Env, token: Address, amount: i128, fee: i128, data: Bytes) -> bool;\n}\n```');
    assert.match(errors.join('\n'), /Rust trait on_flash_loan: signature \(env: Env, token: Address, amount: i128, fee: i128, data: Bytes\) -> bool does not match AMM callback/);
});