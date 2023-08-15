export class CalculatingEngine {
    tokenize(input) {
        const tokens = [];
        let buffer = [];
        let decimalPointCount = 0;
    
        for (let i = 0; i < input.length; i++) {
            let char = input[i];
    
            if ('0123456789'.includes(char)) {
                buffer.push(char);
            } else if (char === '.') {
                decimalPointCount++;
                if (decimalPointCount > 1) {
                    throw new Error("Invalid number with multiple decimal points detected.");
                }
                buffer.push(char);
            } else {
                if (buffer.length) {
                    tokens.push(buffer.join(''));
                    buffer = [];
                    decimalPointCount = 0;
                }
    
                if ('+-*/^()!'.includes(char)) {
                    tokens.push(char);
                } else if (/\s/.test(char)) {
                    continue;
                } else if ('a' <= char && char <= 'z') { 
                    while (i < input.length && 'a' <= input[i] && input[i] <= 'z') {
                        buffer.push(input[i]);
                        i++;
                    }
                    tokens.push(buffer.join(''));
                    buffer = [];
                    i--;
                } else {
                    throw new Error(`Invalid character detected: ${char}`);
                }
            }
        }
    
        if (buffer.length) {
            tokens.push(buffer.join(''));
        }
    
        return tokens;
    }

    toPostfix(tokens) {
        const output = [];
        const operators = [];

        const precedence = {
            '+': 1,
            '-': 1,
            '*': 2,
            '/': 2,
            '^': 3,
            'sin': 4,
            'cos': 4,
            'tan': 4,
            '!': 5
        };

        for (let token of tokens) {
            if ('0123456789.'.includes(token[0])) {
                output.push(token);
            } else if ('+-*/^'.includes(token)) {
                while (operators.length && precedence[operators[operators.length - 1]] >= precedence[token]) {
                    output.push(operators.pop());
                }
                operators.push(token);
            } else if (['sin', 'cos', 'tan'].includes(token)) {
                operators.push(token);
            } else if (token === '(') {
                operators.push(token);
            } else if (token === ')') {
                while (operators.length && operators[operators.length - 1] !== '(') {
                    output.push(operators.pop());
                }
                operators.pop();
            } else if (token === '!') {
                output.push(token);
            } else {
                throw new Error(`Invalid token detected: ${token}`);
            }
        }

        while (operators.length) {
            output.push(operators.pop());
        }

        return output;
    }

    evaluate(input) {
        const tokens = this.tokenize(input);
        const postfix = this.toPostfix(tokens);
        
        const stack = [];

        for (let token of postfix) {
            if ('0123456789.'.includes(token[0])) {
                stack.push(parseFloat(token));
            } else {
                switch (token) {
                    case '+': {
                        const b = stack.pop();
                        const a = stack.pop();
                        stack.push(a + b);
                        break;
                    }
                    case '-': {
                        const b = stack.pop();
                        const a = stack.pop();
                        stack.push(a - b);
                        break;
                    }
                    case '*': {
                        const b = stack.pop();
                        const a = stack.pop();
                        stack.push(a * b);
                        break;
                    }
                    case '/': {
                        const b = stack.pop();
                        if (b === 0) {
                            throw new Error("Division by zero is not allowed!");
                        }
                        const a = stack.pop();
                        stack.push(a / b);
                        break;
                    }
                    case '^': {
                        const b = stack.pop();
                        const a = stack.pop();
                        stack.push(Math.pow(a, b));
                        break;
                    }
                    case 'sin': {
                        const a = stack.pop();
                        stack.push(Math.sin(a));
                        break;
                    }
                    case 'cos': {
                        const a = stack.pop();
                        stack.push(Math.cos(a));
                        break;
                    }
                    case 'tan': {
                        const a = stack.pop();
                        stack.push(Math.tan(a));
                        break;
                    }
                    case '!': {
                        const a = stack.pop();
                        if (a < 0 || Math.floor(a) !== a) {
                            throw new Error("Factorial is only defined for non-negative integers.");
                        }
                        stack.push(this.factorial(a));
                        break;
                    }
                }
            }
        }

        return stack[0];
    }

    factorial(n) {
        if (n === 0) return 1;
        let result = 1;
        for (let i = 1; i <= n; i++) {
            result *= i;
        }
        return result;
    }
}
