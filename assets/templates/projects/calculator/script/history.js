export default class History {
    constructor() {
        this._inputedNumbers = [];
        this._inputedOperators = [];
    }

    set inputedOperators(operator) {
        this._inputedOperators.push(operator);
    }

    get inputedOperators() {
        return this._inputedOperators;
    }

    set inputedNumbers(input) {
        this._inputedNumbers.push(input);
    }

    get inputedNumbers() {
        return this._inputedNumbers;
    }

    reset() {
        this._inputedNumbers = [];
        this._inputedOperators = [];
    }
}