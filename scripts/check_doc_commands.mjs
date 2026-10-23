#!/usr/bin/env node
import { execFileSync } from 'node:child_process';
import { readFileSync, readdirSync } from 'node:fs';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const GLOBAL_FLAGS = new Set(['id', 'network', 'source', 'rpc_url', 'network_passphrase']);
const CONTRACT_HINTS = [
    ['concentrated liquidity', 'concentrated_liquidity'], ['batch auction', 'batch_auction'],
    ['batch router', 'batch_router'], ['dex aggregator', 'dex_aggregator'],
    ['oracle aggregator', 'oracle_aggregator'], ['incentive campaign', 'incentive_campaigns'],
    ['reserve manager', 'reserve_manager'], ['cl position nft', 'cl_position_nft'],
    ['pol vesting', 'pol_vesting'], ['twap consumer', 'twap_consumer'],
    ['twal consumer', 'twal_consumer'], ['v2 to v3 migration', 'v2_to_v3_migration'],
    ['governance', 'governance'], ['factory', 'factory'], ['staking', 'staking'],
    ['token', 'token'], ['router', 'router'], ['auction', 'batch_auction'],
    ['amm', 'amm'], ['pool', 'amm'],
];

function functionEntries(interfaceJson) {
    const entries = Array.isArray(interfaceJson) ? interfaceJson : interfaceJson.functions ?? interfaceJson.contracts ?? [];
    return entries.flatMap((entry) => {
        const fn = entry.function_v0 ?? entry;
        return fn.name && Array.isArray(fn.inputs) ? [fn] : [];
    });
}

function isOptional(input) {
    return typeof input.type === 'object' && input.type !== null && 'option' in input.type;
}

function shellTokens(command) {
    return command.match(/"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|[^\s]+/g) ?? [];
}

function parseFences(markdown) {
    const lines = markdown.split(/\r?\n/);
    const blocks = [];
    let heading = '';
    let annotation = null;
    for (let index = 0; index < lines.length; index += 1) {
        const line = lines[index];
        const headingMatch = line.match(/^#{1,6}\s+(.+?)\s*#*$/);
        if (headingMatch) heading = headingMatch[1];
        const annotationMatch = line.match(/^\s*<!--\s*contract:\s*([\w-]+)\s*-->\s*$/i);
        if (annotationMatch) annotation = annotationMatch[1].replaceAll('-', '_');
        const fenceMatch = line.match(/^\s*(`{3,}|~{3,})([^`]*)$/);
        if (!fenceMatch) continue;
        const fence = fenceMatch[1];
        const language = fenceMatch[2].trim().split(/\s+/)[0].toLowerCase();
        const body = [];
        const startLine = index + 2;
        while (++index < lines.length && !new RegExp(`^\\s*${fence[0]}{${fence.length},}\\s*$`).test(lines[index])) {
            body.push(lines[index]);
        }
        if (['sh', 'shell', 'bash', 'console'].includes(language)) {
            blocks.push({ body, startLine, heading, annotation });
        } else if (language === 'rust') {
            blocks.push({ body, startLine, heading, annotation, language });
        }
        annotation = null;
    }
    return blocks;
}

function commandsFromBlock(block) {
    const commands = [];
    let pending = '';
    let firstLine = 0;
    let annotation = null;
    for (let index = 0; index < block.body.length; index += 1) {
        let line = block.body[index].trim();
        const annotationMatch = line.match(/^#\s*contract:\s*([\w-]+)\s*$/i);
        if (annotationMatch) {
            annotation = annotationMatch[1].replaceAll('-', '_');
            continue;
        }
        if (!pending && (!line || line.startsWith('#'))) continue;
        line = line.replace(/^\$\s+/, '');
        if (!pending) firstLine = block.startLine + index;
        const continues = /\\\s*$/.test(line);
        pending += `${pending ? ' ' : ''}${line.replace(/\s*\\\s*$/, '')}`;
        if (continues) continue;
        if (pending.includes('stellar contract invoke')) {
            commands.push({ text: pending, line: firstLine, annotation });
            annotation = null;
        }
        pending = '';
    }
    if (pending.includes('stellar contract invoke')) commands.push({ text: pending, line: firstLine, annotation });
    return commands;
}

function inferContract(heading, annotation) {
    if (annotation) return annotation;
    const normalized = heading.toLowerCase().replaceAll('_', ' ').replaceAll('-', ' ');
    return CONTRACT_HINTS.find(([hint]) => normalized.includes(hint))?.[1] ?? null;
}

function contractNames() {
    const source = readFileSync(join(ROOT, 'scripts/abi_contracts.sh'), 'utf8');
    const entries = source.match(/ABI_CONTRACTS=\(([\s\S]*?)\)/)?.[1];
    if (!entries) throw new Error('Could not read ABI_CONTRACTS from scripts/abi_contracts.sh');
    return entries.match(/^\s+([a-z0-9_]+)\s*$/gm)?.map((name) => name.trim()) ?? [];
}

function rustTraitMethods(markdown) {
    const methods = [];
    const fence = /```rust\s*([\s\S]*?)```/g;
    for (const match of markdown.matchAll(fence)) {
        const code = match[1];
        const traitPattern = /#\[contractclient(?:\(([^\]]*)\))?\]\s*pub\s+trait\s+(\w+)\s*\{([\s\S]*?)\}/g;
        for (const traitMatch of code.matchAll(traitPattern)) {
            const client = traitMatch[1]?.match(/name\s*=\s*"(\w+)"/)?.[1] ?? traitMatch[2];
            const contract = client.toLowerCase().includes('twap') ? 'twap_consumer'
                : client.toLowerCase().includes('amm') ? 'amm' : null;
            for (const methodMatch of traitMatch[3].matchAll(/fn\s+(\w+)\s*\(([^)]*)\)\s*(?:->\s*([^;]+))?;/g)) {
                const args = methodMatch[2].split(',').map((argument) => argument.trim())
                    .filter(Boolean)
                    .map((argument) => argument.replace(/\s+/g, ' '));
                const offset = match.index + match[0].indexOf(code) + traitMatch.index + traitMatch[0].indexOf(traitMatch[3]) + methodMatch.index;
                methods.push({ name: methodMatch[1], args, returnType: methodMatch[3]?.trim() ?? '', contract, line: markdown.slice(0, offset).split('\n').length });
            }
        }
    }
    return methods;
}

function callbackMethods(markdown) {
    const methods = [];
    for (const match of markdown.matchAll(/```rust\s*([\s\S]*?)```/g)) {
        for (const method of match[1].matchAll(/fn\s+(on_flash_loan)\s*\(([^)]*)\)\s*->\s*([^;{]+);/g)) {
            if (!/\benv\s*:\s*Env\b/.test(method[2])) continue;
            const args = method[2].split(',').map((argument) => argument.trim())
                .filter(Boolean).map((argument) => argument.replace(/\s+/g, ' '));
            const offset = match.index + match[0].indexOf(match[1]) + method.index;
            methods.push({ name: method[1], args, returnType: method[3].trim(), line: markdown.slice(0, offset).split('\n').length });
        }
    }
    return methods;
}

function validateRustSnippets(markdown, file, interfaces) {
    const errors = [];
    const allEntries = Object.entries(interfaces).flatMap(([contract, value]) =>
        functionEntries(value).map((fn) => ({ contract, fn })));
    for (const method of rustTraitMethods(markdown)) {
        const candidates = allEntries.filter(({ fn }) => fn.name === method.name);
        const matches = candidates.filter(({ fn }) =>
            fn.inputs.map((input) => input.name).join(',') === method.args
                .filter((argument) => !argument.startsWith('env:'))
                .map((argument) => argument.match(/^(\w+)\s*:/)?.[1]).join(','));
        if (!matches.length) {
            const expected = candidates.map(({ contract, fn }) => `${contract}: ${fn.inputs.map((input) => input.name).join(', ')}`).join('; ') || 'no ABI method';
            errors.push(`${file}:${method.line}: Rust trait ${method.name}: arguments (${method.args.join(', ')}) do not match ABI parameters (${expected})`);
        } else if (method.contract) {
            const source = readFileSync(join(ROOT, `contracts/${method.contract}/src/lib.rs`), 'utf8');
            const sourceMethod = source.match(new RegExp(`pub\\s+fn\\s+${method.name}\\s*\\(([\\s\\S]*?)\\)\\s*(?:->\\s*([^\\{]+))?\\s*\\{`));
            if (sourceMethod) {
                const sourceArgs = sourceMethod[1].split(',').map((argument) => argument.trim())
                    .filter(Boolean)
                    .map((argument) => argument.replace(/\s+/g, ' '));
                const sourceReturn = sourceMethod[2]?.trim() ?? '';
                if (method.args.join(',') !== sourceArgs.join(',') || method.returnType.replace(/\s+/g, '') !== sourceReturn.replace(/\s+/g, '')) {
                    errors.push(`${file}:${method.line}: Rust trait ${method.name}: signature does not match ${method.contract} contract source (parameters: ${sourceArgs.join(', ') || '(none)'}; return: ${sourceReturn || '()'})`);
                }
            }
        }
    }
    if (callbackMethods(markdown).length) {
        const source = readFileSync(join(ROOT, 'contracts/amm/src/lib.rs'), 'utf8');
        const callback = source.match(/pub trait FlashLoanReceiver\s*\{\s*fn on_flash_loan\s*\(([\s\S]*?)\)\s*->\s*([^;]+);/);
        const expected = callback?.[1].split(',').map((argument) => argument.trim())
            .filter(Boolean).map((argument) => argument.replace(/\s+/g, ' ')) ?? [];
        const expectedReturn = callback?.[2].trim() ?? '';
        for (const method of callbackMethods(markdown)) {
            if (method.args.join(',') !== expected.join(',') || method.returnType.replace(/\s+/g, '') !== expectedReturn.replace(/\s+/g, '')) {
                errors.push(`${file}:${method.line}: Rust trait on_flash_loan: signature (${method.args.join(', ')}) -> ${method.returnType} does not match AMM callback (${expected.join(', ')}) -> ${expectedReturn}`);
            }
        }
    }
    return errors;
}

function parseInvocation(text) {
    const tokens = shellTokens(text);
    const invokeIndex = tokens.findIndex((token, index) => token === 'invoke' && tokens[index - 1] === 'contract');
    if (invokeIndex < 0) return null;
    const delimiterIndex = tokens.indexOf('--', invokeIndex + 1);
    if (delimiterIndex < 0 || !tokens[delimiterIndex + 1]) return null;
    // `$(stellar ... -- fn)` leaves the closing parenthesis (or `;`, `|`) glued to the token.
    const functionName = tokens[delimiterIndex + 1].replace(/^--/, '').replace(/[);|&]+$/, '');
    const flagsBefore = [];
    const flagsAfter = [];
    const collect = (slice, target) => {
        for (let index = 0; index < slice.length; index += 1) {
            if (/^--[\w-]+$/.test(slice[index])) target.push(slice[index].slice(2).replaceAll('-', '_'));
        }
    };
    const preDelimiter = tokens.slice(invokeIndex + 1, delimiterIndex);
    collect(preDelimiter.filter((token) => !GLOBAL_FLAGS.has(token.replace(/^--/, '').replaceAll('-', '_'))), flagsBefore);
    collect(tokens.slice(delimiterIndex + 2), flagsAfter);
    return { functionName, flagsBefore, flagsAfter };
}

export function validateMarkdown(markdown, file, interfaces) {
    const errors = [];
    for (const block of parseFences(markdown)) {
        for (const command of commandsFromBlock(block)) {
            const invocation = parseInvocation(command.text);
            if (!invocation) continue;
            const { functionName, flagsBefore, flagsAfter } = invocation;
            const indicatedContract = inferContract(block.heading, command.annotation ?? block.annotation);
            let candidates = indicatedContract && interfaces[indicatedContract]
                ? [{ contract: indicatedContract, fn: functionEntries(interfaces[indicatedContract]).find((fn) => fn.name === functionName) }].filter(({ fn }) => fn)
                : Object.entries(interfaces).flatMap(([contract, value]) => functionEntries(value)
                    .filter((fn) => fn.name === functionName).map((fn) => ({ contract, fn })));
            if (!indicatedContract && candidates.length > 1) {
                const supplied = new Set(flagsAfter.filter((flag) => !GLOBAL_FLAGS.has(flag)));
                const compatible = candidates.filter(({ fn }) => supplied.size > 0 && [...supplied].every((flag) => fn.inputs.some((input) => input.name === flag)));
                if (compatible.length === 1) candidates = compatible;
            }
            if (candidates.length !== 1) {
                const contracts = candidates.map(({ contract }) => contract).join(', ') || 'none';
                errors.push(`${file}:${command.line}: ${functionName}: ${candidates.length ? `ambiguous contract (${contracts}); annotate with <!-- contract: name -->` : `unknown function; matching contracts: ${contracts}`}`);
                continue;
            }
            const { contract, fn } = candidates[0];
            const parameters = fn.inputs;
            const names = new Set(parameters.map((input) => input.name));
            const abiNames = parameters.map((input) => `--${input.name}`).join(', ') || '(none)';
            for (const flag of flagsBefore) {
                if (!GLOBAL_FLAGS.has(flag)) {
                    errors.push(`${file}:${command.line}: ${functionName} (${contract}): contract flag --${flag} is before --; ABI parameter: ${flag}`);
                }
            }
            for (const flag of flagsAfter) {
                if (GLOBAL_FLAGS.has(flag)) {
                    errors.push(`${file}:${command.line}: ${functionName} (${contract}): global flag --${flag.replaceAll('_', '-')} is after --; move it before -- (ABI parameters: ${abiNames})`);
                } else if (!names.has(flag)) {
                    errors.push(`${file}:${command.line}: ${functionName} (${contract}): unknown flag --${flag}; ABI parameters: ${abiNames}`);
                }
            }
            for (const input of parameters) {
                if (!isOptional(input) && !flagsAfter.includes(input.name)) {
                    errors.push(`${file}:${command.line}: ${functionName} (${contract}): missing required flag --${input.name}; ABI parameter: ${input.name}`);
                }
            }
        }
    }
    errors.push(...validateRustSnippets(markdown, file, interfaces));
    return errors;
}

function markdownFiles(directory) {
    return readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
        const path = join(directory, entry.name);
        return entry.isDirectory() ? markdownFiles(path) : entry.isFile() && entry.name.toLowerCase() === 'readme.md' || entry.isFile() && path.endsWith('.md') ? [path] : [];
    });
}

function loadInterfaces(wasmDir, stellar) {
    return Object.fromEntries(contractNames().map((contract) => {
        const wasm = join(wasmDir, `${contract}.wasm`);
        const output = execFileSync(stellar, ['-q', 'contract', 'info', 'interface', '--wasm', wasm, '--output', 'json'], { encoding: 'utf8' });
        return [contract, JSON.parse(output)];
    }));
}

function main() {
    const wasmDir = resolve(process.argv[2] ?? join(ROOT, 'target/wasm32v1-none/release'));
    let interfaces;
    try {
        interfaces = loadInterfaces(wasmDir, process.env.STELLAR_BIN ?? 'stellar');
    } catch (error) {
        console.error(`ERROR: Could not load contract interfaces from ${wasmDir}: ${error.message}`);
        process.exitCode = 2;
        return;
    }
    const roots = ['README.md', 'docs', 'contracts', 'packages', 'examples'];
    const files = roots.flatMap((path) => {
        const absolute = join(ROOT, path);
        try {
            return path.endsWith('.md') ? [absolute] : markdownFiles(absolute);
        } catch {
            return [];
        }
    });
    const errors = files.flatMap((file) => validateMarkdown(readFileSync(file, 'utf8'), relative(ROOT, file), interfaces));
    if (errors.length) {
        errors.forEach((error) => console.error(error));
        process.exitCode = 1;
    } else {
        console.log(`Checked contract invoke examples in ${files.length} Markdown files.`);
    }
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main();