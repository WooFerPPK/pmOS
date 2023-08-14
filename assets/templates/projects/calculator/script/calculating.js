export default class Calculating {
    constructor(numbers = [], operators = []) {
        this._answer = 0;
        let currentResult = parseFloat(numbers[0]);
        for (var index = 0; index < numbers.length; index++) {
            let currentNumber = parseFloat(numbers[index]);
            let error = false;
            switch (operators[index-1]) {
                case 'add':
                    currentResult += currentNumber;
                    break;
                case 'subtract':
                    currentResult -= currentNumber;
                    break;
                case 'multiply':
                    currentResult *= currentNumber;
                    break;
                case 'divide':
                    if (currentNumber !== 0) {
                        currentResult /= currentNumber;
                    } else {
                        error = true;
                        currentResult = "undefined";
                    }
                    break;
                default:
                    currentResult = currentNumber;
            }
            if (error) {
                break;
            }
        }
        this._answer = currentResult;
    }

    get answer() {
        return this._answer;
    }
}