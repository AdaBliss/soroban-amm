import { createRequire } from "node:module";
import { writeFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";

// Use the repository's pinned reference implementation without adding a
// runtime dependency to the dependency-free Go module.
const require = createRequire(
  new URL("../../../services/graphql-api/package.json", import.meta.url),
);
const { Address, nativeToScVal, xdr } = require("@stellar/stellar-sdk");

const ACCOUNT = "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF";
const CONTRACT = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4";

const value = (type, data) => ({ type, value: data });
const symbol = (text) => value("symbol", text);
const i128 = (text) => value("i128", text);

function encode(logical) {
  switch (logical.type) {
    case "bool":
    case "u32":
    case "i32":
    case "u64":
    case "i64":
    case "u128":
    case "i128":
    case "bytes":
    case "string":
    case "symbol":
      return nativeToScVal(
        logical.type === "bytes"
          ? Buffer.from(logical.value, "base64")
          : ["u64", "i64", "u128", "i128"].includes(logical.type)
            ? BigInt(logical.value)
            : logical.value,
        { type: logical.type },
      );
    case "void":
      return xdr.ScVal.scvVoid();
    case "address":
      return Address.fromString(logical.value).toScVal();
    case "vec":
      return xdr.ScVal.scvVec(logical.value.map(encode));
    case "map":
      return xdr.ScVal.scvMap(
        logical.value.map(({ key, value: mapValue }) =>
          new xdr.ScMapEntry({ key: encode(key), val: encode(mapValue) })
        ),
      );
    case "error":
      return xdr.ScVal.scvError(xdr.ScError.sceContract(logical.value));
    default:
      throw new Error(`unsupported fixture type: ${logical.type}`);
  }
}

const map = (entries) => value(
  "map",
  entries.map(([key, mapValue]) => ({ key: symbol(key), value: mapValue })),
);

const poolInfo = map([
  ["admin", value("address", ACCOUNT)],
  ["fee_bps", i128("30")],
  ["fee_recipient", value("address", ACCOUNT)],
  ["flash_loan_fee_bps", i128("5")],
  ["lp_rebate_bps", i128("2")],
  ["protocol_fee_bps", i128("3")],
  ["reserve_a", i128("10000000")],
  ["reserve_b", i128("25000000")],
  ["token_a", value("address", CONTRACT)],
  ["token_b", value("address", ACCOUNT)],
  ["total_shares", i128("15000000")],
]);

const swapQuote = map([
  ["amount_out", i128("975000")],
  ["effective_price", i128("9750000")],
  ["fee_amount", i128("3000")],
  ["price_impact_bps", i128("25")],
  ["spot_price", i128("10000000")],
]);

const logicalVectors = [
  ["bool_true", value("bool", true)],
  ["bool_false", value("bool", false)],
  ["void", value("void", null)],
  ["u32_max", value("u32", 4_294_967_295)],
  ["i32_min", value("i32", -2_147_483_648)],
  ["u64_max", value("u64", "18446744073709551615")],
  ["i64_min", value("i64", "-9223372036854775808")],
  ["u128_zero", value("u128", "0")],
  ["u128_max", value("u128", "340282366920938463463374607431768211455")],
  ["i128_negative", i128("-42")],
  ["i128_min", i128("-170141183460469231731687303715884105728")],
  ["i128_max", i128("170141183460469231731687303715884105727")],
  ["bytes_empty", value("bytes", "")],
  ["bytes_binary", value("bytes", "AP8Q")],
  ["string", value("string", "soroban amm")],
  ["symbol_long", symbol("abcdefghijklmnopqrstuvwxyz012345")],
  ["account_address", value("address", ACCOUNT)],
  ["contract_address", value("address", CONTRACT)],
  ["vec_empty", value("vec", [])],
  ["vec_nested", value("vec", [value("vec", [value("u32", 1)]), value("vec", [])])],
  ["map_empty", value("map", [])],
  ["map_nested", map([["items", value("vec", [map([["amount", i128("9")]])])]])],
  ["get_info", poolInfo],
  ["simulate_swap", swapQuote],
  ["contract_error", value("error", 7), true],
];

const vectors = logicalVectors.map(([name, logical, unsupported = false]) => ({
  name,
  xdr: encode(logical).toXDR("base64"),
  logical,
  unsupported,
}));

const output = `${JSON.stringify({ generatedBy: "@stellar/stellar-sdk", vectors }, null, 2)}\n`;
await writeFile(fileURLToPath(new URL("scval_vectors.json", import.meta.url)), output);
