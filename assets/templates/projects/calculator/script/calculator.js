import Insert from './insert.js';
import Display from './display.js';
import History from './history.js';
import Calculating from './calculating.js';

export default class Calculator {
    constructor(calculatorClass, shadowRoot) {
        this.shadowRoot = shadowRoot
        this._calculatorClass = calculatorClass;
        this.insert = new Insert();
        this.history = new History();
        this.display = new Display(this.calculatorClass, this.history, shadowRoot);
        this.assignEventListeners(this.calculatorClass);
        this.reset = false;
    }

    get calculatorClass() {
        return this._calculatorClass;
    }

    assignEventListeners(calculatorClass) {
        let combined = [...this.shadowRoot.querySelectorAll('.' + calculatorClass + ' .number')].concat([...this.shadowRoot.querySelectorAll('.' + calculatorClass + ' .operator')]);
        for (let index = 0; index < combined.length; index++) {
            combined[index].addEventListener('click', (element) => {
                this.inputPressed(element.target.value, element.target.classList.value);
            });
        }
    }

    inputPressed(value, operator) {
        switch(operator) {
            case 'number':
                this.numberPressed(value);
            break;
            case 'operator':
                this.operatorPressed(value);
            break;
            default:
                console.error(`Input error - Value: ${value}, Operator: ${operator}`);
        }
    }

    numberPressed(value) {
        this.insert.input += value;

        //Checks to see if 0 is the first character. If so then remove the 0 from the string.
        if (this.insert.input.charAt(0) == 0) {
            this.insert.input = this.insert.input.substring(1, this.insert.input.length);
        }

        this.display.currentInput = this.insert.input;
        this.display.output = this.display.currentExpression;
        this.display.showOutput();

        this.setOutputDisplay();
    }

    operatorPressed(operator) {
        if (operator === 'back' && this.reset === true) {
            this.resetCalculator();
        } else if (operator === 'back') {
            let skipOperatorCheck = false;
            if (this.insert.input) {
                this.insert.input = this.insert.input.slice(0, -1);
                skipOperatorCheck = true;
            } 
            if (skipOperatorCheck === false && this.history.inputedOperators.length > 0) {
                this.history.inputedOperators.pop();
                this.insert.input = this.history.inputedNumbers.pop();
            }
            this.display.currentInput = this.insert.input;
            this.setOutputDisplay();
        } else {
            this.history.inputedNumbers.push(this.insert.input);
            this.history.inputedOperators.push(operator);
            this.insert.reset();
        }
              
        if (operator === "equal") {
            let calculate = new Calculating(this.history.inputedNumbers, this.history.inputedOperators);
            this.setOutputDisplay(`${this.display.currentExpression} = ${calculate.answer}`);
            this.history.reset();
            this.reset = true;
        } else if (operator != 'back') {
            this.display.currentInput = '';
            this.setOutputDisplay();
        }
    }

    resetCalculator() {
        this.insert.reset();
        this.history.reset();
        this.display.reset();
        this.reset = false;
    }

    setOutputDisplay(outputOveride) {
        if (outputOveride) {
            this.display.output = outputOveride
        } else {
            this.display.output = this.display.currentExpression;
        }
        this.display.showOutput();
    }
}