#!/usr/bin/env node
import { readFileSync } from "node:fs";
//#region src/calc.ts
function fail(message) {
	process.stderr.write(`${message}\n`);
	process.exit(1);
}
function evaluate(expression) {
	const tokens = expression.match(/\d[\d,]*(?:\.\d+)?|\.\d+|\S/gu) ?? [];
	let position = 0;
	function primary() {
		if (position >= tokens.length) throw new Error("Expected a number.");
		const token = tokens[position];
		position += 1;
		if (token === "(") {
			const value = sum();
			if (tokens[position] !== ")") throw new Error("Expected `)`.");
			position += 1;
			return value;
		}
		if (!/^[\d.]/u.test(token)) throw new Error(`Unexpected \`${token}\`.`);
		return Number(token.replaceAll(",", ""));
	}
	function power() {
		const base = primary();
		if (tokens[position] !== "^") return base;
		position += 1;
		return base ** unary();
	}
	function unary() {
		if (tokens[position] === "-" || tokens[position] === "+") {
			const sign = tokens[position] === "-" ? -1 : 1;
			position += 1;
			return sign * unary();
		}
		return power();
	}
	function product() {
		let value = unary();
		while (tokens[position] === "*" || tokens[position] === "/") {
			const operator = tokens[position];
			position += 1;
			value = operator === "*" ? value * unary() : value / unary();
		}
		return value;
	}
	function sum() {
		let value = product();
		while (tokens[position] === "+" || tokens[position] === "-") {
			const operator = tokens[position];
			position += 1;
			value = operator === "+" ? value + product() : value - product();
		}
		return value;
	}
	const value = sum();
	if (position < tokens.length) throw new Error(`Unexpected \`${tokens[position]}\`.`);
	if (!Number.isFinite(value)) throw new Error("The result isn't a finite number.");
	return value;
}
const lines = readFileSync(0, "utf8").split("\n");
if (lines.at(-1) === "") lines.pop();
if (lines.length === 1 && !lines[0].includes("=")) lines[0] += " =";
for (const [index, line] of lines.entries()) {
	const equals = line.indexOf("=");
	if (equals === -1) {
		process.stdout.write(`${line}\n`);
		continue;
	}
	const expression = line.slice(0, equals).trimEnd();
	try {
		process.stdout.write(`${expression} = ${Number(evaluate(expression).toPrecision(12))}\n`);
	} catch (error) {
		fail(`Line ${index + 1}: ${error instanceof Error ? error.message : String(error)}`);
	}
}
//#endregion
export {};
